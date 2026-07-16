# reeve-ingestion

How telemetry gets in. The OTLP receiver on 4317, the Anthropic HTTP
proxy on 4318 that synthesizes spans from raw API traffic, and the
four-stage pipeline (receive, normalize, assemble, route) that turns
either stream into trace trees. Pricing and outbound secret scanning
live here too, because both happen on the way through.

Part of [Reeve](https://github.com/Dancode-188/reeve), the terminal
cockpit for AI agents.
