# Embedded Python runtime

`python-embed.zip` is the unmodified official CPython 3.14.10 Windows x64
embeddable distribution. It is compiled into the portable executable and
unpacked on first use under the AgentCockpit data directory. This supports
local usage statistics without installing Python or downloading anything at
runtime. The archive includes `LICENSE.txt` with the Python Software
Foundation License and notices for included third-party libraries.

- Official release page: https://www.python.org/downloads/release/python-31410/
- Download: https://www.python.org/ftp/python/3.14.10/python-3.14.10-embed-amd64.zip
- Size: 12,619,664 bytes
- SHA-256: `90905e64a840557d8c1bc4f5b74db3d7ee85e3ffb217611cac1c7813acea4159`

Keep the archive tracked in source control so offline release builds do not
depend on a developer's Python installation. The executable contains the
archive, and the extracted copy is verified against it before use. The local
unpacked runtime occupies about 30 MB; the archive includes its license.

## Bundled agenthub runtime

`agenthub.py` and `console.html` are the MIT-licensed
[agent-memory-suite](https://github.com/shen1950/agent-memory-suite) backend
used for check-in and local quota reads. They are compiled into the executable
and written next to the unpacked Python runtime.

`agenthub-libs.zip` adds `cryptography` 49.0.0, `cffi` 2.1.0 and
`pycparser` 3.0 under `Lib/site-packages`, plus a `python314._pth` that puts
that directory on `sys.path`. Licenses are included in `licenses/` inside the
archive and in `docs/third-party/`. The archive is verified during extraction;
nested paths are checked for zip-slip before writing.
