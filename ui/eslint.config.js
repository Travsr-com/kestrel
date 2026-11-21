import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';
import svelteConfig from './svelte.config.js';

export default ts.config(
  {
    // The a11y lint test lints `tests/fixtures` on purpose, with `ignore: false`.
    ignores: ['dist/', 'coverage/', 'node_modules/', 'tests/fixtures/'],
  },
  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs.recommended,
  {
    languageOptions: {
      globals: { ...globals.browser, ...globals.node },
    },
  },
  {
    files: ['**/*.svelte', '**/*.svelte.ts'],
    languageOptions: {
      parserOptions: {
        parser: ts.parser,
        extraFileExtensions: ['.svelte'],
        svelteConfig,
      },
    },
  },
  {
    files: ['**/*.svelte'],
    rules: {
      // Svelte's compiler a11y warnings (alt text, labels, roles, keyboard handlers) are errors.
      'svelte/valid-compile': ['error', { ignoreWarnings: false }],
    },
  },
);
