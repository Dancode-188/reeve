import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import pytest

from tests._otel import EXPORTER


@pytest.fixture(autouse=True)
def clear_spans():
    EXPORTER.clear()
    yield
