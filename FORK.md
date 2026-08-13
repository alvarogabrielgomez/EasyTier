# Kanpachi fork of EasyTier

Part of **[Kanpachi Protection](https://github.com/alvarogabrielgomez/kanpachi/blob/main/kanpachi-protection.md)**:
*everything the game did not ask for is closed on the virtual adapter.*

This is a fork of [EasyTier/EasyTier](https://github.com/EasyTier/EasyTier),
maintained for [Kanpachi](https://github.com/alvarogabrielgomez/kanpachi) and
consumed by its
[engine](https://github.com/alvarogabrielgomez/kanpachi-engine).

It started for exactly one reason: upstream opens the virtual adapter in the
Windows Firewall while creating it, which is the opposite of that promise, and
no configuration turns it off. Everything else upstream does is kept.

**The `kanpachi` branch is upstream plus the changes listed below, and nothing
else.** That claim is meant to be checked, not believed:

```
git diff v2.6.4 kanpachi -- '*.rs' '*.proto'
# five files changed, and every hunk is listed below
```

## Branch or tag, and which one to use

**Kanpachi follows the `kanpachi` branch, which moves.** This fork is not
versioned and the product that consumes it is at v0, so a tag per patch set
bought a number nobody read at the price of force-pushing it every time the
fork changed. Following a moving branch costs nothing there, because
`Cargo.lock` pins the commit and a build never consults the branch: what
consults it is `cargo update`, which is somebody's deliberate act.

**To pin instead of follow, use the `v2.6.4-kanpachi` tag**, a snapshot of this
branch that never moves. When the branch moves on and somebody needs a pin at
the new point, that is the moment to cut a tag for it and to decide how the
series is named. Deciding it now costs a naming scheme and buys nothing.

Outside the source there are two added documents, this one and a note at the top
of `README.md` saying that this is a fork and pointing at the original.

Nothing of Kanpachi's own lives here, and that is deliberate: the value of this
repository is that its diff against upstream reads in one glance, and code of
ours would bury it. What is added is generic — a credential's expiry can be
pushed forward — and carries no idea of rooms, invite codes or games.

## Changelog against upstream

Newest first, and named by commit. These entries used to be titled by tag, and
the tags are gone for the reason above.

### Adds `renew_credential` (commit `c98aa15`)

One method added to the credential manager, and its RPC.

| Added | Where | What it does |
|---|---|---|
| `CredentialManager::renew_credential(id, ttl)` | `easytier/src/peers/credential_manager.rs` | Moves an existing credential's `expiry_unix` to `now + ttl`, **keeping its keypair**, and returns the new expiry |
| `rpc RenewCredential` | `easytier/src/proto/api_instance.proto` | Exposes it on `CredentialManageRpc`, implemented in both `peers/rpc_service.rs` and `rpc_service/credential_manage.rs` |

#### Why it cannot be done with what upstream has

`generate_credential` early-returns when the id already exists, so it cannot
extend one. The alternative is revoke-and-reissue, and that changes the keypair:
the credential secret **is** the holder's x25519 Noise static key, so a new one
means the holder is a different peer, has to be re-trusted, and loses its
session. For pushing an expiry forward that is the whole cost of the operation
and none of the benefit.

Expiry is enforced continuously and not only at handshake —
`collect_trusted_credentials` drops expired entries and
`disconnect_untrusted_peers` acts on that — so without this, every credential
ends its holder's session at its TTL no matter how long the network has been up.

#### What it deliberately does not do

It emits no `CredentialChanged` event. Nothing about who is trusted changed, and
`update_my_peer_info_routine` already republishes the trusted set every second,
so the new expiry propagates within about a second on its own. Measured against
a real binary: renewing a credential five seconds from death revived it, and the
new expiry was counted from the moment of the call, not stacked on the old one.

### Removes the firewall calls, which is why this fork exists (commit `42894f5`, from upstream `v2.6.4` at `8428a89`)

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
