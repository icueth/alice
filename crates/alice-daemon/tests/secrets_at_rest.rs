//! Secrets at rest (owner's decision 2026-10-17): a person's store keeps credentials as written
//! when `privacy.redact` is off (on a server tenant: when the admin pinned
//! `ALICE_PRIVACY_REDACT=false`), the shared team store always redacts, and nothing sent to a
//! cloud service carries a secret, whatever the store keeps.
//!
//! Secret-shaped strings are built with `concat!`, so no scanner takes them for real ones.

mod support;

use alice_core::api::{GetRequest, OutputFormat, RecallMode, RecallRequest, RememberRequest};
use std::path::Path;
use support::caddy::Server;

/// A GitHub-shaped token.
const TOKEN: &str = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
/// The admin's `alice-server secrets keep`, as the tenant file pins it.
const KEEP: &[(&str, &str)] = &[("ALICE_PRIVACY_REDACT", "false")];

/// Whether any file under `dir` holds `needle` (the database and its WAL included).
fn tree_contains(dir: &Path, needle: &str) -> bool {
    std::fs::read_dir(dir).unwrap().any(|e| {
        let p = e.unwrap().path();
        if p.is_dir() {
            tree_contains(&p, needle)
        } else {
            std::fs::read(&p)
                .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
                .unwrap_or(false)
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keep_tenant_stores_and_recalls_a_secret_as_written_and_the_team_store_never_holds_one() {
    let s = Server::start_with_team_and_env(&["ann"], KEEP).await;
    let ann = s.tenant("ann");
    assert!(
        !ann.svc.settings().privacy.redact,
        "the admin's pin applies"
    );

    // The person's own store: as written, and recalled as written.
    let r = ann
        .svc
        .remember(RememberRequest {
            content: format!("The deploy bot pushes with the token {TOKEN} on the ops box"),
            project_hint: Some("ops".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let id = r.id.unwrap();
    let got = ann
        .svc
        .get(GetRequest {
            ids: vec![id.clone()],
            include_provenance: false,
        })
        .await
        .unwrap();
    assert!(
        got.memories[0].memory.body.contains(TOKEN),
        "stored as written"
    );
    let found = ann
        .svc
        .recall(RecallRequest {
            query: "deploy bot token ops box".into(),
            project: Some("ops".into()),
            mode: Some(RecallMode::Fast),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        found
            .results
            .iter()
            .any(|h| h.id == id && h.body.as_deref().is_some_and(|b| b.contains(TOKEN))),
        "recalled as written: {:?}",
        found.results
    );
    assert!(tree_contains(ann.home.root(), TOKEN));

    // A shared write leaves redacted: everyone reads the team store.
    let shared = ann
        .svc
        .remember(RememberRequest {
            content: format!("Team CI pushes with {TOKEN} from the runner"),
            shared: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(shared.shared);
    let team = s.team.as_ref().unwrap();
    let held = team
        .svc
        .get(GetRequest {
            ids: vec![shared.id.unwrap().trim_start_matches("team:").to_string()],
            include_provenance: false,
        })
        .await
        .unwrap();
    let body = &held.memories[0].memory.body;
    assert!(
        body.contains("[REDACTED:github_token]") && !body.contains(TOKEN),
        "{body}"
    );
    // The team store redacts what reaches it any other way too, pin or no pin.
    team.svc
        .remember(RememberRequest {
            content: format!("Straight to the team store: {TOKEN}"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !tree_contains(team.home.root(), TOKEN),
        "the team store holds no secret"
    );
}
