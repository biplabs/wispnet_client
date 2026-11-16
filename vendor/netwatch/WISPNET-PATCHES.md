# Wispnet iOS patch

Vendored from crates.io `netwatch 0.19.3`, preserving the upstream licenses.

iOS uses netdev address enumeration without BSD routing sockets or macOS
route-table ABI declarations. Default-route and router metadata is unavailable.
The native runner uses NWPathMonitor and Endpoint::network_change for changes.
Other platform implementations are unchanged.
