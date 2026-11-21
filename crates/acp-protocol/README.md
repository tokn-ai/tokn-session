# tokn-acp-protocol

Current standard ACP v1 initialization, session, prompt, configuration and
permission wire contracts, independent of providers and interaction-core.
Unknown optional fields remain tolerated and standard `_meta` fields remain
opaque. Configuration selectors support flat or grouped choices and preserve
opaque option IDs.

Provider-specific metadata interpretation and legacy APIs belong in the
provider adapter. This crate does not model legacy `models` catalogs,
`session/set_model`, or nonstandard prompt-result fields.

Contract: [Agent Client Protocol v1](https://agentclientprotocol.com/protocol/v1/overview).
