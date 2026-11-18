# Security

## Reporting a vulnerability

Report vulnerabilities privately on GitHub: open the repository's **Security** tab and
choose **Report a vulnerability**
([direct link](https://github.com/tembo/voxl/security/advisories/new)). Please don't
open a public issue. Say which version (`voxl --version`) and platform you found it on,
and how to reproduce it. We'll confirm we got it and keep you posted while we fix it.

## Supported versions

Fixes go into the latest release, so upgrade to get them.

## What voxl protects, and what it leaves to you

Whoever gets a session gets the desktop voxl streams: they see the screen, type, click,
use the clipboard and move files. voxl's one door is ssh, and it adds no login or
encryption of its own: ssh already decides who gets in and encrypts the control stream,
the tunnel the lane crosses encrypts it, and every other hop is on one machine. So:

- **ssh is the door:** a session starts as `ssh <host> voxl attach`, and whoever ssh lets
  in as the daemon's user gets the desktop. Keep that account's keys as you would its
  shell.
- **The daemon listens on no network port:** its three sockets are unix sockets. The
  control socket is mode 0600 and takes a connection only from the daemon's own user
  (checked by the peer's uid); anything else is closed unread. The two lane sockets are
  mode 0660, for a group the server's proxy runs in, which reaches them as
  `localhost:6901`.
- **Lanes:** for each control connection the daemon makes a 256-bit capability from the
  system's random source and sends it as the session's first control message, over ssh.
  A lane's first message must present it, with its generation; a wrong or stale
  one gets the lane closed or the datagram dropped, without a reply, and the daemon acts
  on nothing else a lane sends. A datagram lane belongs to the session by the address its
  attach came from. Lanes carry video and audio one way, from the daemon to the relay:
  input, clipboard, files and the microphone go only over ssh. The capability is never in
  argv, the environment, a file or a log line, is compared in constant time and zeroed
  when it drops, and both ends turn core dumps off. When the control stream closes, the
  capability is forgotten and the lane attached with it is dropped.
- **Agent commands:** `voxl stats`, `voxl screenshot` and `voxl record`, run over ssh,
  ask the daemon through the same control socket, so the same uid rule admits them. Each
  is answered once on its own connection; none gets a capability or reaches a lane.
- **Malformed input:** every reader of a lane message, a control message or an attach
  checks lengths before it reads, has a fixed maximum, and returns a typed error; none
  allocates by a length it read.
- **The relay:** `voxl relay` serves the page on `127.0.0.1` only, and answers only
  requests for an IP address or `localhost` from its own page, turning away other sites'
  pages (and DNS rebinding) that browsers would let connect to it. Anyone who can reach
  127.0.0.1 on the viewer's machine can use its session, so run the relay on a machine
  only you use.
- **Files:** the Files panel only reaches `$HOME`; nothing outside it is reachable.
