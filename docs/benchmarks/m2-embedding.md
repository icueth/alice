# M2: HTTP embedding providers and the model switch

Embeddings are local by default (`embedding.provider = "local"`, multilingual-e5-small, int8).
Three HTTP providers can replace it: `ollama`, `openai` (any OpenAI-compatible endpoint) and
`voyage`. Intel macOS and musl Linux builds, which have no local model, use one of them.

```toml
[embedding]
provider = "ollama"            # local | ollama | openai | voyage
model    = "nomic-embed-text"  # the provider's model name; the local provider runs one model only
endpoint = "http://127.0.0.1:11434/v1"   # optional; each provider has a default
```

`openai` and `voyage` need a key: `alice config set-secret embedding_api_key` (or the
`ALICE_EMBEDDING_API_KEY` variable). It is sent only as an `Authorization: Bearer` header to the
provider's endpoint, with the `User-Agent` `alice/<version>`. It is never part of a log line,
an error, `alice status`, `/api/v1/health` or the settings view. A key set for OpenAI is never
sent to a local Ollama.

## What leaves the machine

An embedder is **local** when the provider is `local`, or its endpoint host is `localhost` or a
loopback address (`127.0.0.1`, `::1`, ...): a local Ollama. Anything else is **remote**. A
local embedder sees every project's text, as it always did. A remote one sees a text only if
the **cloud policy** allows its project (`privacy.cloud_denied_projects`, a project's
`.alice.toml` `cloud_denied`; every name a project has is asked, and a degraded configuration
says no):

- **Memories and chunks** of a denied project are not sent. Their `embed_state` becomes
  `skipped_cloud` ("held back by the cloud policy"): they stay searchable by words, they are
  not a backlog (`embedding backlog` ignores them), and `alice status` counts them. They are
  queued again (`pending`) when the policy lets their project through or the embedder becomes
  a local one; the embedding worker looks for that every five minutes and at once after a change
  of `embedding.*` or `privacy.*`.
- **A recall's query** is sent only if the recall's project is allowed, and then only after the
  project's secret patterns have been applied to it. For a denied project the recall runs on
  words alone and says so (`notes` in the JSON answer, a line at the end of the text).
- **Switching** to a remote provider (or to another remote host) is a loosening of a
  protection: through `alice config set` it applies at once with a note; an edit of `config.toml`
  waits for three identical polls. The answer also states how many memories and chunks will be
  sent (an estimate) and how many the policy keeps at home.

## Changing the model

A change of `embedding.provider`, `embedding.model` or `embedding.endpoint` applies without a
restart:

1. The model in force keeps serving searches and embedding new text.
2. A background re-embed fills the new model's vectors (`embeddings` rows under the new model id)
   and its index (`vectors/<model>.memories.usearch`, `.chunks.bq`; `:` and other characters a
   file name cannot hold become `_`, with a short hash added so two ids never share files). It
   waits for requests to be over, so it does not slow a recall. Progress is in `meta`
   (`reembed.<model>.cursor.memory`/`.chunk`) and in `alice status`.
3. When everything has a vector, the embedding worker is paused for a moment, a final pass embeds
   what was written or edited meanwhile, and the model, the embedder and the index are replaced
   together. The old model's vectors stay until the daily `model_purge` job deletes them (and its
   index files). Switching back before then starts fresh: the old vectors may be stale.
4. A failure leaves the old model in force and says why (`embedding_error` in health). A provider
   that is down is waited for; a daemon that starts while it is down keeps the old vectors
   searchable and tries again every minute.

A change made while the daemon is stopped is picked up at the next start, and a re-embed that was
interrupted resumes from its cursors. One re-embed runs at a time; a newer change replaces it.

`alice config test embedding` embeds one short text with the model in force.
