CLI Quickstart Tutorial
=======================

This tutorial walks through installing the ``grepster`` command-line
tool and running your first indexed search. By the end you will have
indexed a project and issued a ranked query.

Prerequisites
-------------

You need a 64-bit machine and roughly 200 MB of free disk for the index.
No network access is required after installation — everything runs
locally.

Installation
------------

Install the binary and confirm it is on your ``PATH``:

- Download the release archive for your platform.
- Extract it and move ``grepster`` into ``/usr/local/bin``.
- Run ``grepster --version`` to verify the install.

Building an Index
-----------------

Point the indexer at a directory. The first run is *full*; later runs
are incremental and only touch changed files.

::

   grepster index ./src --exclude target --threads 4

The ``--exclude`` flag accepts a glob and may be repeated. Indexing is
**idempotent** — re-running never corrupts an existing index.

Query Flags
-----------

============= ========= ============================
Flag          Default   Description
============= ========= ============================
``--limit``   ``20``    Maximum results returned
``--case``    ``smart`` Case sensitivity mode
``--context`` ``2``     Lines of surrounding context
``--json``    ``false`` Emit machine-readable output
============= ========= ============================

Running a Search
----------------

Search combines a literal term with an optional path filter:

::

   grepster query "parse_header" --limit 5 --context 3

Results are ranked by term frequency and recency. Pass ``--json`` when
piping into another tool so you never parse the human-formatted table by
hand.
