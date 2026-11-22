import js from '@eslint/js';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';
import tseslint from 'typescript-eslint';

// Storage policy (design §5.7, WI 0160 C4): app code never touches persistent
// browser storage. sessionStorage is allowed only through
// src/lib/state/session.ts; IndexedDB only through the saved signing-key
// module src/lib/client/credentialStore.ts. localStorage and cookies: never.
const storageBans = [
  { name: 'localStorage', message: 'Persistent storage is forbidden (design §5.7).' },
  { name: 'indexedDB', message: 'Persistent storage is forbidden (design §5.7).' },
  { name: 'sessionStorage', message: 'Use src/lib/state/session.ts (design §5.7).' },
];
const storageSyntax = [
  { selector: "MemberExpression[property.name='localStorage']", message: 'Persistent storage is forbidden (design §5.7).' },
  { selector: "MemberExpression[property.name='indexedDB']", message: 'Persistent storage is forbidden (design §5.7).' },
  { selector: "MemberExpression[object.name='document'][property.name='cookie']", message: 'Cookies are forbidden (design §5.7).' },
  { selector: "MemberExpression[property.name='sessionStorage']", message: 'Use src/lib/state/session.ts (design §5.7).' },
  { selector: "JSXAttribute[name.name='dangerouslySetInnerHTML']", message: 'No raw HTML: all daemon/agent text is untrusted (design §5.5).' },
  { selector: "JSXAttribute[name.name='style']", message: 'Inline styles are blocked by the production CSP (style-src \'self\'); use CSS classes.' },
  { selector: "CallExpression[callee.name='eval']", message: 'eval is forbidden by the CSP.' },
  { selector: "NewExpression[callee.name='Function']", message: 'new Function is forbidden by the CSP.' },
];

export default tseslint.config(
  { ignores: ['node_modules/**', '.next/**', 'out/**', 'next-env.d.ts', 'e2e/**', 'edge/**', 'acceptance/**', 'public/sw.js'] },
  js.configs.recommended,
  ...tseslint.configs.strict,
  {
    files: ['**/*.{ts,tsx}'],
    plugins: { 'react-hooks': reactHooks },
    languageOptions: { globals: { ...globals.browser } },
    rules: {
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'error',
      'no-restricted-globals': ['error', ...storageBans],
      'no-restricted-syntax': ['error', ...storageSyntax],
      '@typescript-eslint/no-explicit-any': 'error',
      '@typescript-eslint/consistent-type-imports': 'error',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
    },
  },
  {
    // No frame payloads or debug traces from the client library (design §5.7).
    files: ['src/lib/**/*.{ts,tsx}'],
    rules: { 'no-console': 'error' },
  },
  {
    files: ['src/lib/state/session.ts'],
    rules: {
      'no-restricted-globals': ['error', ...storageBans.filter(ban => ban.name !== 'sessionStorage')],
      'no-restricted-syntax': ['error', ...storageSyntax.filter(rule => !rule.selector.includes('sessionStorage'))],
    },
  },
  {
    // WI 0160 C4: the one credential-persistence module (non-extractable key + public metadata).
    files: ['src/lib/client/credentialStore.ts'],
    rules: {
      'no-restricted-globals': ['error', ...storageBans.filter(ban => ban.name !== 'indexedDB')],
      'no-restricted-syntax': ['error', ...storageSyntax.filter(rule => !rule.selector.includes('indexedDB'))],
    },
  },
  {
    files: ['src/sw/**/*.ts'],
    languageOptions: { globals: { ...globals.serviceworker } },
  },
  {
    files: ['scripts/**/*.mjs', '*.config.{ts,mjs}'],
    languageOptions: { globals: { ...globals.node } },
  },
  {
    // Tests may stub storage to prove the app never writes it.
    files: ['test/**/*.{ts,tsx}'],
    languageOptions: { globals: { ...globals.node, ...globals.browser } },
    rules: { 'no-restricted-globals': 'off', 'no-restricted-syntax': 'off', '@typescript-eslint/no-non-null-assertion': 'off' },
  },
);
