# Browser acceptance runner

This isolated npm package pins Playwright 1.58.2, TypeScript 5.8.3 and Node
types. It drives the exported web app (`web/out`) and the real daemon binary; it
does not implement or replace the web app.

From the repository root:

```sh
cargo build --workspace --bins
make web/reinstall
npm --prefix web run check                     # produces web/out
npm --prefix web/e2e exec -- playwright install --with-deps chromium firefox
npm --prefix web/e2e run check
npm --prefix web/e2e test
```

`make test-web` runs the web and edge checks, builds the binary and then runs
this suite; it is the CI gate (`web` job in `.github/workflows/test.yml`).

The runner uses fresh isolated homes and the real binary at `target/debug/awman`.
Ports 3000 and 9877 must be free. It never attaches to an existing daemon. Test
origins are set through `awman config set`; production defaults are unchanged.
All four projects run serially: Chromium and Firefox, each at 1280×800 and
390×844 (touch enabled at narrow width). Browser traces, videos and screenshots
are disabled because they can contain grants, pairing secrets and session bytes.
`script` from util-linux supplies a PTY for explicit owner approval; there is no
noninteractive approval bypass. No Docker or real agent is run by this suite.

Specs:

- `transport.spec.ts` exercises browser-to-daemon fetch/CORS using a plainly
  named fixture page, not the app: default-off, stopped, the live recipe and an
  idempotent recheck; exact origin, origin replacement and the rule that browser
  requests never fall back to API keys. `npm --prefix web/e2e run test:transport` runs
  only this subset.
- `link.spec.ts` exercises the browser link flow against the real daemon:
  creation from a gesture, CLI confirmation with the displayed Origin, listener
  and scope, approval and denial, one-use polling, launch-context access with the
  grant and grant revocation through `awman daemon api browsers.delete`.
  Clock-controlled Rust and browser-client unit tests cover code expiry.
- `app.spec.ts` loads the exported app from `web/out` and checks the connect
  screen's recipe states (daemon stopped, engine off, listener off) and the
  recheck and link initiation controls. It fails explicitly if `web/out` is
  absent.
- `csp.spec.ts` serves the app under the exact production Content Security
  Policy from `web/edge/src/security.ts`: the shipped terminal WASM core must
  compile, the same core must be refused without `'wasm-unsafe-eval'` (negative
  control), and the shell must load with zero violations.

Scope limits. The fixture server speaks plain HTTP on `localhost:3000`; it does
not stand in for the deployed HTTPS origin, the Cloudflare Worker or an
installed service worker (service workers are blocked in these runs). Engine
action-trace parity between the TUI and the web app, live agent PTY rendering,
lag recovery with a real container, multiple competing browser frontends and the
offline install/update lifecycle are covered by unit and component tests under
`web/test`, not by this suite. Do not read a passing run as coverage of those.
