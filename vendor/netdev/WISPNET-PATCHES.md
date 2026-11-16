# Wispnet iOS patch

Vendored from crates.io `netdev 0.46.3`, preserving the upstream licenses.

The Darwin IPv6 ioctl uses a `libc::in6_ifreq` type not exposed on iOS.
iOS leaves optional IPv6 flags at their unavailable defaults while retaining
getifaddrs address enumeration. macOS retains its original ioctl implementation.
