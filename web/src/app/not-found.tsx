// Replaces Next's default 404, which ships inline styles that the production
// CSP (`style-src 'self'`) forbids.
export default function NotFound() {
  return (
    <main className="not-found">
      <h1>Not found</h1>
      <p>
        This address is not part of awman. <a href="/">Open awman</a>
      </p>
    </main>
  );
}
