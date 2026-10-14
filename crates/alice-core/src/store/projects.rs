//! Projects and their aliases.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::normalize::{sha256_hex, slugify};
use crate::project::ProjectRef;
use rusqlite::{Connection, OptionalExtension, params};

/// Keys starting with this belong to projects created by an import before any checkout existed.
pub const PLACEHOLDER_KEY_PREFIX: &str = "import:";

pub const GLOBAL_ID: &str = "00000000000000000000GLOBAL";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedProject {
    pub id: String,
    pub slug: String,
    pub name: String,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ResolvedProject> {
    Ok(ResolvedProject {
        id: r.get(0)?,
        slug: r.get(1)?,
        name: r.get(2)?,
    })
}

pub fn get_by_id(conn: &Connection, id: &str) -> Result<ResolvedProject> {
    conn.query_row(
        "SELECT id, slug, name FROM projects WHERE id = ?1",
        [id],
        row,
    )
    .optional()?
    .ok_or_else(|| AliceError::NotFound(format!("project {id}")))
}

/// Exact match on an already-slugified string.
fn find_by_slug(conn: &Connection, slug: &str) -> Result<Option<ResolvedProject>> {
    Ok(conn
        .query_row(
            "SELECT id, slug, name FROM projects WHERE slug = ?1",
            [slug],
            row,
        )
        .optional()?)
}

/// True when `s` already resolves to a project, as a slug or as an alias.
fn slug_taken(conn: &Connection, s: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT (SELECT count(*) FROM projects WHERE slug = ?1) + (SELECT count(*) FROM project_aliases WHERE alias = ?1)",
        [s],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Insert the project if its key is new; returns the stored project either way.
/// A slug already in use gets a suffix from the key's hash, lengthened until it is free.
pub fn upsert_project(conn: &Connection, p: &ProjectRef) -> Result<ResolvedProject> {
    if let Some(found) = find_by_key(conn, &p.key)? {
        // The working tree may have moved (a re-clone, a new worktree): the rules of its
        // `.alice.toml` are read from the live root, so a root that exists replaces the stored one.
        if let Some(root) = p.root.as_ref().filter(|r| r.exists()) {
            let root = root.to_string_lossy();
            conn.execute(
                "UPDATE projects SET root_path = ?1 WHERE id = ?2 AND root_path IS NOT ?1",
                params![root, found.id],
            )?;
        }
        return Ok(found);
    }
    let base = slugify(&p.slug);
    if base.is_empty() {
        return Err(AliceError::invalid(format!(
            "project slug {:?} is empty after normalisation; use letters or digits",
            p.slug
        )));
    }
    if !p.key.starts_with(PLACEHOLDER_KEY_PREFIX) {
        // A real project adopts the placeholder of the same slug (or an alias equal to it).
        let placeholder: Option<ResolvedProject> = conn
            .query_row(
                "SELECT id, slug, name FROM projects WHERE slug = ?1 AND key LIKE 'import:%'
                 UNION ALL
                 SELECT p.id, p.slug, p.name FROM project_aliases a JOIN projects p ON p.id = a.project_id
                  WHERE a.alias = ?1 AND p.key LIKE 'import:%'
                 LIMIT 1",
                [&base],
                row,
            )
            .optional()?;
        if let Some(found) = placeholder {
            let root = p
                .root
                .as_ref()
                .filter(|r| r.exists())
                .map(|r| r.to_string_lossy().into_owned());
            conn.execute(
                "UPDATE projects SET key = ?1, name = ?2, git_remote = ?3, root_path = COALESCE(?4, root_path) WHERE id = ?5",
                params![p.key, p.name, p.remote, root, found.id],
            )?;
            return Ok(ResolvedProject {
                name: p.name.clone(),
                ..found
            });
        }
    }
    let slug = if slug_taken(conn, &base)? {
        let hash = sha256_hex(p.key.as_bytes());
        // Only another project's slug or an alias can hold a candidate, so this ends early.
        let mut free = None;
        for n in [6, 8, 12, 16, hash.len()] {
            let candidate = format!("{base}-{}", &hash[..n]);
            if !slug_taken(conn, &candidate)? {
                free = Some(candidate);
                break;
            }
        }
        free.ok_or_else(|| {
            AliceError::Other(format!(
                "every slug for project {:?} is taken; remove a conflicting alias and retry",
                p.key
            ))
        })?
    } else {
        base
    };
    let id = new_id();
    conn.execute(
        "INSERT INTO projects (id, key, slug, name, git_remote, root_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            p.key,
            slug,
            p.name,
            p.remote,
            p.root.as_ref().map(|r| r.to_string_lossy().into_owned()),
            now_rfc3339()
        ],
    )?;
    Ok(ResolvedProject {
        id,
        slug,
        name: p.name.clone(),
    })
}

/// Resolve a slug or any registered alias (case and separator insensitive).
pub fn resolve_slug(conn: &Connection, slug_or_alias: &str) -> Result<Option<ResolvedProject>> {
    let s = slugify(slug_or_alias);
    if let Some(p) = find_by_slug(conn, &s)? {
        return Ok(Some(p));
    }
    Ok(conn
        .query_row(
            "SELECT p.id, p.slug, p.name FROM project_aliases a JOIN projects p ON p.id = a.project_id WHERE a.alias = ?1",
            [&s],
            row,
        )
        .optional()?)
}

/// Point `alias` (slugified) at a project, moving it if it already names another project.
pub fn add_alias(conn: &Connection, alias: &str, project_id: &str) -> Result<()> {
    let a = slugify(alias);
    if a.is_empty() {
        return Err(AliceError::invalid(
            "alias is empty after normalisation; use letters or digits",
        ));
    }
    let target = get_by_id(conn, project_id)?;
    if let Some(owner) = find_by_slug(conn, &a)? {
        // Slugs win over aliases in `resolve_slug`, so such an alias could never resolve.
        if owner.id == target.id {
            return Ok(());
        }
        return Err(AliceError::invalid(format!(
            "alias \"{a}\" is already the slug of project \"{}\"; choose another alias",
            owner.slug
        )));
    }
    conn.execute(
        "INSERT INTO project_aliases (alias, project_id) VALUES (?1, ?2) ON CONFLICT(alias) DO UPDATE SET project_id = excluded.project_id",
        params![a, project_id],
    )?;
    Ok(())
}

/// Look up a project by its stable key without creating it (read paths).
pub fn find_by_key(conn: &Connection, key: &str) -> Result<Option<ResolvedProject>> {
    Ok(conn
        .query_row(
            "SELECT id, slug, name FROM projects WHERE key = ?1",
            [key],
            row,
        )
        .optional()?)
}

/// The working-tree root a project was detected from, if it has one. `project_id_or_slug` is
/// an id, a slug or an alias.
pub fn root_of(conn: &Connection, project_id_or_slug: &str) -> Result<Option<String>> {
    let by_id: Option<Option<String>> = conn
        .query_row(
            "SELECT root_path FROM projects WHERE id = ?1",
            [project_id_or_slug],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(root) = by_id {
        return Ok(root);
    }
    let Some(p) = resolve_slug(conn, project_id_or_slug)? else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT root_path FROM projects WHERE id = ?1",
            [&p.id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// The root of the project a memory (named by its id or legacy id) belongs to.
pub fn root_of_memory(conn: &Connection, memory_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT p.root_path FROM memories m JOIN projects p ON p.id = m.project_id
             WHERE m.id = ?1 OR m.legacy_id = ?1 ORDER BY m.seq LIMIT 1",
            [memory_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

pub fn list_projects(conn: &Connection) -> Result<Vec<ResolvedProject>> {
    let mut stmt = conn.prepare("SELECT id, slug, name FROM projects ORDER BY slug")?;
    let rows = stmt
        .query_map([], row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::open_writer;
    use std::path::PathBuf;

    fn conn() -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        (d, c)
    }

    fn pref(key: &str, slug: &str) -> ProjectRef {
        ProjectRef {
            key: key.into(),
            slug: slug.into(),
            name: slug.into(),
            root: None,
            remote: None,
        }
    }

    #[test]
    fn a_real_project_adopts_the_import_placeholder_of_the_same_slug() {
        let (_d, conn) = conn();
        let placeholder = ProjectRef {
            key: "import:pg-ai".into(),
            slug: "pg-ai".into(),
            name: "pg-ai".into(),
            root: None,
            remote: None,
        };
        let imported = upsert_project(&conn, &placeholder).unwrap();
        let real = ProjectRef {
            key: "github.com/icueth/pg-ai".into(),
            slug: "pg-ai".into(),
            name: "PG AI".into(),
            root: None,
            remote: Some("github.com/icueth/pg-ai".into()),
        };
        let adopted = upsert_project(&conn, &real).unwrap();
        assert_eq!(
            adopted.id, imported.id,
            "same project, no pg-ai-<hash> twin"
        );
        assert_eq!(adopted.slug, "pg-ai");
        assert_eq!(
            adopted.name, "PG AI",
            "the real project's name replaces the placeholder's"
        );
        assert_eq!(
            find_by_key(&conn, "github.com/icueth/pg-ai")
                .unwrap()
                .unwrap()
                .id,
            imported.id
        );
        assert!(find_by_key(&conn, "import:pg-ai").unwrap().is_none());
    }

    #[test]
    fn a_placeholder_is_not_adopted_by_another_placeholder_or_a_different_slug() {
        let (_d, conn) = conn();
        let a = upsert_project(&conn, &pref("import:alpha", "alpha")).unwrap();
        let other = upsert_project(&conn, &pref("path:/x/beta", "beta")).unwrap();
        assert_ne!(a.id, other.id);
        let again = upsert_project(&conn, &pref("import:alpha", "alpha")).unwrap();
        assert_eq!(again.id, a.id);
    }

    #[test]
    fn a_placeholder_is_also_adopted_through_an_alias_equal_to_the_real_slug() {
        let (_d, conn) = conn();
        let imported = upsert_project(&conn, &pref("import:alice", "alice")).unwrap();
        add_alias(&conn, "project-alice", &imported.id).unwrap();
        let adopted =
            upsert_project(&conn, &pref("github.com/icueth/alice", "project-alice")).unwrap();
        assert_eq!(adopted.id, imported.id, "no project-alice-<hash> twin");
    }

    #[test]
    fn upsert_is_stable_and_registers_slug_alias() {
        let (_d, c) = conn();
        let a = upsert_project(&c, &pref("github.com/me/app", "app")).unwrap();
        let b = upsert_project(&c, &pref("github.com/me/app", "app")).unwrap();
        assert_eq!(a, b);
        assert_eq!(resolve_slug(&c, "APP").unwrap().unwrap().id, a.id);
    }

    #[test]
    fn slug_collision_gets_a_suffix() {
        let (_d, c) = conn();
        let a = upsert_project(&c, &pref("github.com/me/api", "api")).unwrap();
        let b = upsert_project(&c, &pref("github.com/other/api", "api")).unwrap();
        assert_eq!(a.slug, "api");
        assert_ne!(b.slug, "api");
        assert!(b.slug.starts_with("api-"), "{}", b.slug);
    }

    #[test]
    fn aliases_fold_name_variants() {
        let (_d, c) = conn();
        let p = upsert_project(&c, &pref("github.com/me/pg-ai", "pg-ai")).unwrap();
        add_alias(&c, "wing_pg_ai", &p.id).unwrap();
        assert_eq!(resolve_slug(&c, "pg_ai").unwrap().unwrap().id, p.id);
        assert_eq!(resolve_slug(&c, "Wing-PG-AI").unwrap().unwrap().id, p.id);
        assert!(resolve_slug(&c, "nope").unwrap().is_none());
    }

    #[test]
    fn global_always_exists() {
        let (_d, c) = conn();
        assert_eq!(resolve_slug(&c, "global").unwrap().unwrap().id, GLOBAL_ID);
        assert_eq!(
            upsert_project(&c, &ProjectRef::global()).unwrap().id,
            GLOBAL_ID
        );
    }

    #[test]
    fn suffixed_slug_that_is_taken_gets_a_longer_suffix() {
        let (_d, c) = conn();
        let short = |key: &str| format!("api-{}", &sha256_hex(key.as_bytes())[..6]);
        let first = upsert_project(&c, &pref("k/one", "api")).unwrap();
        // Another project already owns the slug the second "api" would get ...
        let squatter = upsert_project(&c, &pref("k/squat", &short("k/two"))).unwrap();
        assert_eq!(squatter.slug, short("k/two"));
        let second = upsert_project(&c, &pref("k/two", "api")).unwrap();
        // ... and an alias owns the one the third would get.
        add_alias(&c, &short("k/three"), &first.id).unwrap();
        let third = upsert_project(&c, &pref("k/three", "api")).unwrap();

        let slugs = [&first.slug, &squatter.slug, &second.slug, &third.slug];
        for (i, s) in slugs.iter().enumerate() {
            assert!(s.starts_with("api"), "{s}");
            assert!(!slugs[..i].contains(s), "duplicate slug {s}");
        }
        assert_ne!(third.slug, short("k/three"));
        assert_eq!(
            resolve_slug(&c, &short("k/three")).unwrap().unwrap().id,
            first.id
        );
        assert_eq!(
            resolve_slug(&c, &second.slug).unwrap().unwrap().id,
            second.id
        );
        assert_eq!(resolve_slug(&c, &third.slug).unwrap().unwrap().id, third.id);
    }

    #[test]
    fn empty_slug_is_rejected() {
        let (_d, c) = conn();
        let err = upsert_project(&c, &pref("k/x", "!!!")).unwrap_err();
        assert!(matches!(err, AliceError::Invalid(_)), "{err}");
        assert!(find_by_key(&c, "k/x").unwrap().is_none());
    }

    #[test]
    fn add_alias_checks_project_and_slug_ownership() {
        let (_d, c) = conn();
        let api = upsert_project(&c, &pref("k/api", "api")).unwrap();
        let web = upsert_project(&c, &pref("k/web", "web")).unwrap();
        let err = add_alias(&c, "x", "01NOSUCHPROJECT0000000000").unwrap_err();
        assert!(matches!(err, AliceError::NotFound(_)), "{err}");
        // The slug of another project would never resolve to the alias target.
        let err = add_alias(&c, "API", &web.id).unwrap_err();
        assert!(matches!(err, AliceError::Invalid(_)), "{err}");
        assert_eq!(resolve_slug(&c, "api").unwrap().unwrap().id, api.id);
        // A project's own slug is already an alias of itself.
        add_alias(&c, "Web", &web.id).unwrap();
        assert!(matches!(
            add_alias(&c, "--", &web.id).unwrap_err(),
            AliceError::Invalid(_)
        ));
        // Aliases can be moved to another project.
        add_alias(&c, "frontend", &api.id).unwrap();
        add_alias(&c, "frontend", &web.id).unwrap();
        assert_eq!(resolve_slug(&c, "frontend").unwrap().unwrap().id, web.id);
    }

    #[test]
    fn lookups_by_key_and_id_and_listing() {
        let (_d, c) = conn();
        assert!(find_by_key(&c, "github.com/me/app").unwrap().is_none());
        let p = ProjectRef {
            key: "github.com/me/app".into(),
            slug: "app".into(),
            name: "App".into(),
            root: Some(PathBuf::from("/work/app")),
            remote: Some("github.com/me/app".into()),
        };
        let app = upsert_project(&c, &p).unwrap();
        assert_eq!(app.name, "App");
        assert_eq!(find_by_key(&c, &p.key).unwrap(), Some(app.clone()));
        assert_eq!(get_by_id(&c, &app.id).unwrap(), app);
        assert!(matches!(
            get_by_id(&c, "01NOSUCHPROJECT0000000000").unwrap_err(),
            AliceError::NotFound(_)
        ));
        let (remote, root): (Option<String>, Option<String>) = c
            .query_row(
                "SELECT git_remote, root_path FROM projects WHERE id = ?1",
                [&app.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(remote.as_deref(), Some("github.com/me/app"));
        assert_eq!(root.as_deref(), Some("/work/app"));

        upsert_project(&c, &pref("k/zeta", "zeta")).unwrap();
        let slugs: Vec<_> = list_projects(&c)
            .unwrap()
            .into_iter()
            .map(|p| p.slug)
            .collect();
        assert_eq!(slugs, ["app", "global", "zeta"]);
    }

    #[test]
    fn root_of_finds_a_root_by_id_slug_or_alias() {
        let (_d, c) = conn();
        let mut with_root = pref("k/one", "one");
        with_root.root = Some(PathBuf::from("/work/one"));
        let one = upsert_project(&c, &with_root).unwrap();
        let none = upsert_project(&c, &pref("k/two", "two")).unwrap();
        add_alias(&c, "uno", &one.id).unwrap();
        for name in [one.id.as_str(), "one", "uno", "UNO"] {
            assert_eq!(root_of(&c, name).unwrap().as_deref(), Some("/work/one"));
        }
        assert_eq!(root_of(&c, &none.slug).unwrap(), None);
        assert_eq!(root_of(&c, "missing").unwrap(), None);
    }

    #[test]
    fn a_moved_root_replaces_the_stored_one_when_it_exists() {
        let (d, c) = conn();
        let old = d.path().join("old");
        let new = d.path().join("new");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        let mut r = pref("k/app", "app");
        r.root = Some(old.clone());
        let p = upsert_project(&c, &r).unwrap();
        assert_eq!(root_of(&c, &p.id).unwrap().as_deref(), old.to_str());
        r.root = Some(new.clone());
        upsert_project(&c, &r).unwrap();
        assert_eq!(root_of(&c, &p.id).unwrap().as_deref(), new.to_str());
        // A root that is not there does not replace a good one.
        r.root = Some(d.path().join("gone"));
        upsert_project(&c, &r).unwrap();
        assert_eq!(root_of(&c, &p.id).unwrap().as_deref(), new.to_str());
    }
}
