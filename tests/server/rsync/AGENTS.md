# rsync server tests

No LLM calls; a python policy plays the model. The policy offers modules `pub` and `mirror`.
`pub` holds:

- a text file with a set mtime;
- a binary file (hex content, mode 600);
- a 100 KiB file;
- a nested tree;
- a symlink;
- an empty file.

Any other module is refused.

`real_client_test.rs` drives the **stock rsync 3.2.7 client**, which speaks protocol 29 to
the daemon (`apt-get install rsync`). It fails rather than skips without it.

- The module list, with the MOTD.
- `--list-only`: one level, sizes. With `-l`, the symlink's target appears.
- `-a pub/`: every file compared byte for byte, the symlink, the mtime and both modes.
- One named file with no options.
- A second `-a --checksum` run onto a changed tree. The client offers block checksums, which
  are ignored, and the result is still identical.
- A dry run, which writes nothing.
- Refusals:
  - an unknown module (the model's `rsync_refuse`);
  - a missing path (exit 23, `link_stat`);
  - an upload (`read only`);
  - `-z` (refused).

`wire_test.rs` covers the codec alone:

- The whole-file MD4 against bytes captured from a stock exchange.
- The 12-byte longint.
- rsync's sort order.
- A file list round trip through demultiplexing, with frames split mid-field, a keep-alive and
  an info message.
- Unsafe names refused.
