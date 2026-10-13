# References

AirPlay 2 has no official public spec. These are the community sources we follow.

| Source | Use it for |
|---|---|
| OwnTones — github.com/owntone/owntone-server (`src/outputs/airplay.c`, `src/outputs/pair_ap/`) | Working C **sender** that streams to HomePods: pairing, SETUP plists, RTP. Closest reference to what we build. |
| pyatv — github.com/postlund/pyatv (`pyatv/protocols/raop/`, `pyatv/auth/hap_*`) | Readable Python sender; transient pairing and audio streaming. Used in the M0 spike. |
| shairport-sync — github.com/mikebrady/shairport-sync | **Receiver** side; useful to understand what the HomePod expects. |
| nqptp — github.com/mikebrady/nqptp | PTP timing as used by AirPlay 2. |
| "AirPlay 2 Internals" — emanuelecozzi.net/docs/airplay2 | Protocol walkthrough: pairing, RTSP, feature bits. |
| openairplay spec — openairplay.github.io/airplay-spec | Legacy AirPlay 1 / RAOP details, TXT keys, feature bits. |

Captured traces from our own devices go in `captures/` (git-ignored).
