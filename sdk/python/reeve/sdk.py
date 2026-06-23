"""
ReeveClient: core SDK class.
"""


class ReeveClient:
    """
    Connect a Python agent to the Reeve intervention channel.

    Not yet implemented. Coming in v0.3.0.
    """

    def __init__(self, host: str = "localhost", port: int = 4316) -> None:
        self.host = host
        self.port = port

    async def checkpoint(self, span_id: str) -> None:
        """
        Yield control to Reeve at a safe point in agent execution.

        Called by agents at natural stopping points to allow Reeve to
        pause, redirect, or inject context before continuing.
        """
        raise NotImplementedError("coming in v0.3.0")
