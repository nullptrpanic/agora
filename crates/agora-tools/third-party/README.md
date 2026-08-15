# Vendored Browser Assets

The Trace Viewer embeds these browser distributions so it has no CDN or Node.js runtime dependency:

| Package | Version | Included files | Upstream |
| --- | --- | --- | --- |
| `@xterm/xterm` | 6.0.0 | `xterm/xterm.js`, `xterm/xterm.css`, `xterm/LICENSE` | <https://www.npmjs.com/package/@xterm/xterm/v/6.0.0> |
| `@xterm/addon-fit` | 0.11.0 | `xterm-addon-fit/addon-fit.js`, `xterm-addon-fit/LICENSE` | <https://www.npmjs.com/package/@xterm/addon-fit/v/0.11.0> |

The files are copied without source changes from the published package archives. Their SHA-256
digests are:

```text
14903579ff54664cd72f8e8699e6961a6272c21863ec1c3b118cdc8af5d4a972  xterm/xterm.js
854a7c0fb70e8b1a083c16797ab827299fb18744f5ad34f227b48337e33293c6  xterm/xterm.css
ba3ea256ce0620a0992a197d6c9baea64823fc93d8da07a9e366ca9943c18527  xterm-addon-fit/addon-fit.js
```
