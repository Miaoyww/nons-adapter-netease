# NonsPlayer transport patch

Origin: https://github.com/SPlayer-Dev/ncm-api-rs, commit `133b65bfe482e41ebccf018870d3fce07bf58eb3` (the previous Cargo pin). Upstream source and WTFPL license are preserved; server capability remains unused by the host.

- `request.rs`: every client builder leaves the request deadline to the caller and sets an explicit bounded redirect policy (10 hops); `resolve_url` follows the same policy, validates HTTP(S), and returns the final URL for playback delivery. Body reads validate Content-Length and accumulated chunks before JSON/EAPI decoding. Response size is unbounded by default; `set_response_limit` can tighten it per client.
- `error.rs`: typed `ResponseTooLarge`, mapped to the uniform `invalidData` error without response text.
- `api/playlist_detail.rs`: honor an explicit `n` song-body limit (0–100000), retaining the upstream 100000 default for other callers. The adapter requests `n=0,s=0` when reading metadata/IDs and fetches song bodies only for the requested page. Full ID lists, metadata, transport limits and credential handling remain intact.
- No new platform protocol or cryptography implementation. Rebase these small changes when updating upstream, and run the redirect and bounded-body tests here plus the host adapter/playback checks.

The validated accumulation does not bound response payload size unless a limit is set; TLS/HTTP allocations, JSON expansion, and process memory remain caller concerns. Cancellation drops the async body reader; no response or credentials are logged by the patch.
