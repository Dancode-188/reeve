"""
Reeve Python SDK: connect Python agents to the Reeve cockpit.
"""

# Lockstep with the workspace crates: the SDK's compatibility is with
# a specific cockpit, and this is the one place the number lives. The
# handshake and pyproject both follow it. Defined before the sdk import,
# which reads it back for the handshake.
__version__ = "0.6.0"

from .sdk import AgentKilled, CheckpointResult, ReeveSdk

__all__ = ["ReeveSdk", "CheckpointResult", "AgentKilled"]
