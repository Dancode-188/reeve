# reeve-intervention

The control plane. The gRPC control server on 4316, the dispatcher
that owns command delivery and the ack ladder (received, applying,
applied), and the append-only audit log that records every command
forever.

Part of [Reeve](https://github.com/Dancode-188/reeve), the terminal
cockpit for AI agents.
