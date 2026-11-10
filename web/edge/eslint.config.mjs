import js from '@eslint/js';
import ts from 'typescript-eslint';
export default ts.config(
  js.configs.recommended,
  ...ts.configs.recommended,
  { files: ['**/*.ts', '**/*.mjs'], rules: { 'no-console': 'error' } },
  { files: ['scripts/*.mjs'], languageOptions: { globals: { process: 'readonly', URL: 'readonly' } } },
);
