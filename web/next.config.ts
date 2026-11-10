import type { NextConfig } from 'next';

// Static, client-rendered export only (design §5.1): no SSR, Server Actions,
// API routes, middleware or server-side daemon fetches. Everything the app
// knows about a session comes from the same-machine daemon at runtime.
const config: NextConfig = {
  output: 'export',
  trailingSlash: true,
  images: { unoptimized: true },
  reactStrictMode: true,
  poweredByHeader: false,
  productionBrowserSourceMaps: false,
  devIndicators: false,
  allowedDevOrigins: process.env.WEB_DEV_TAILNET_IP ? [process.env.WEB_DEV_TAILNET_IP, '127.0.0.1'] : undefined,
  // Lint runs as its own step of `pnpm run check`.
  eslint: { ignoreDuringBuilds: true },
  // Deterministic build id: the service worker version is derived from the
  // content hashes of the exported files (scripts/build-sw.mjs).
  generateBuildId: async () => 'awman-web',
  compiler: {
    // No frame payloads or debug traces in production bundles (design §5.7).
    removeConsole: { exclude: ['error'] },
  },
  experimental: {
    // CSS must ship as files: the edge CSP is `style-src 'self'` (§5.8).
    inlineCss: false,
  },
};

export default config;
