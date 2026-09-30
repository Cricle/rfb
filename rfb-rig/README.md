# rfb-rig

Rig adapters for RFB: expose an RFB sandbox to [Rig](https://github.com/0xPlaygrounds/rig)
agents as callable tools.

```toml
[dependencies]
rfb-rig = "0.0.1"
```

The crate bridges a sandbox handle (forkd or ZeroBoot) into Rig tool definitions:
guest exec, filesystem reads/writes, and stream sessions are surfaced as
typed tools with the same contracts as the `rfb-sdk` facade
(`sdk/UNIFIED_API.md`). See the workspace README for sandbox provisioning and
the CLI reference (`rfb` crate README) for host-side setup.

License: MIT OR Apache-2.0.
