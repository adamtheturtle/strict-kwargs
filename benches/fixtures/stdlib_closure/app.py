"""Large standard-library import closure.

Exercises the embedded-typeshed indexing path (issue #30): every import
below pulls a stdlib module (and its own re-export closure) into the
DefinitionIndex, and the calls resolve through that index via the built-in
resolver. No third-party or first-party dependencies, so the workload is
fully reproducible from the vendored typeshed alone.
"""

import argparse
import base64
import collections
import dataclasses
import datetime
import functools
import hashlib
import itertools
import json
import logging
import os
import pathlib
import re
import string
import subprocess
import textwrap
import urllib.parse


def build() -> None:
    parser = argparse.ArgumentParser("demo")
    parser.add_argument("--name")

    encoded = base64.b64encode(b"payload")
    digest = hashlib.sha256(b"payload")

    counter = collections.Counter([1, 2, 2, 3])
    ordered = collections.OrderedDict()

    moment = datetime.datetime(2026, 5, 17)
    delta = datetime.timedelta(7)

    payload = json.dumps({"name": "demo"})
    parsed = json.loads(payload)

    here = pathlib.Path("src")
    joined = os.path.join("a", "b")

    pattern = re.compile("[a-z]+")
    matched = pattern.match("abc")

    wrapped = textwrap.fill("a long sentence to wrap", 20)
    parts = urllib.parse.urlsplit("https://example.com/path?q=1")

    logger = logging.getLogger("demo")
    logger.info("done")

    chained = itertools.chain([1], [2])
    reduced = functools.reduce(lambda a, b: a + b, [1, 2, 3])
    completed = subprocess.run(["true"], check=True)

    template = string.Template("$x")

    _ = (
        encoded,
        digest,
        counter,
        ordered,
        moment,
        delta,
        parsed,
        here,
        joined,
        matched,
        wrapped,
        parts,
        chained,
        reduced,
        completed,
        template,
    )


@dataclasses.dataclass
class Record:
    name: str
    count: int
