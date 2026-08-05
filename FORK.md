# Kanpachi fork of EasyTier

This is a fork of [EasyTier/EasyTier](https://github.com/EasyTier/EasyTier),
maintained for [Kanpachi](https://github.com/alvarogabrielgomez/kanpachi).

**Every tag published here is upstream plus the changes listed below, and
nothing else.** That claim is meant to be checked, not believed:

```
git diff v2.6.4 v2.6.4-kanpachi.1
```

## Changelog against upstream

### `v2.6.4-kanpachi.1` — from upstream `v2.6.4` (commit `8428a89`)

Two calls removed from `easytier/src/instance/virtual_nic.rs`. Both wrote
Windows Firewall ALLOW rules, by COM, from inside network startup.

| Removed | Was at | What it wrote |
|---|---|---|
| `add_self_to_firewall_allowlist()` | `create_tun`, line 531 | Inbound and outbound ALLOW for the running executable, **any protocol, every interface on the machine** |
| `add_interface_to_firewall_allowlist(&ifname)` | `create_dev`, line 698 | Inbound and outbound ALLOW on the virtual interface: TCP, UDP, ICMP, and one rule for **any protocol**, no port and no address restriction |

The functions themselves are left in `easytier/src/arch/windows.rs`, unused and
still public. Deleting them would grow the diff for no gain, and a consumer
that wants the old behaviour can still call them.

#### Why

Kanpachi's whole reason to exist is that the virtual adapter is born closed:
only the ports the active game profile asks for are opened, only toward the IP
addresses of members currently in the room, and only on the machine hosting.
An unconditional "allow everything on this interface" rule undoes that in the
same layer Kanpachi uses to grant access.

The rules also outlive the process. They are permanent Windows Firewall rules,
grouped under `EasyTier`, and they survive a reboot and an uninstall of the
program that caused them.

Upstream's stated purpose for them is subnet proxy and KCP proxy. Kanpachi runs
with `proxy_cidrs` cleared and `enable_kcp_proxy` off, so neither applies.

#### Why a fork rather than configuration

There is no way to turn these off from outside. Both calls sit inside
`#[cfg(target_os = "windows")]` and are reached unconditionally from
`NetworkInstance::start()`, so they run on the library path as well as the CLI
one. No cargo feature, no config field, and no environment variable changes
them.

#### Safety of the removal

Upstream already treats the failure of both calls as non-fatal: each was
wrapped in a `match` whose error arm only logs a warning and continues. Their
absence is a state upstream already handles.

## Licence

EasyTier is LGPL-3.0. This fork is distributed under the same terms, and the
source of every published tag is here in full, which is what LGPL-3.0 §4
requires of a modified library.
