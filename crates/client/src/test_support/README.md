# Publication test peer

`public_ingress.rs` is a frozen server-side test peer used only by the
`public_access` unit tests. Production ingress moved to the independent
`wispnet_server/apps/public-gateway/src/ingress.rs` project.

This fixture lets client tests run without another checkout. After changes to
publication framing, authorization, routing or upgrades, run the real-binary
cross-project test in the server project and update this fixture deliberately.
