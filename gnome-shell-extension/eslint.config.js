import js from '@eslint/js';

// GNOME Shell runs the extension in GJS: ES modules, with the shell's `global` object and
// the usual console and timer functions.
export default [
    js.configs.recommended,
    {
        files: ['typewhisper@typewhisper.com/**/*.js'],
        languageOptions: {
            ecmaVersion: 'latest',
            sourceType: 'module',
            globals: {
                global: 'readonly',
                console: 'readonly',
                TextDecoder: 'readonly',
                TextEncoder: 'readonly',
                setTimeout: 'readonly',
                clearTimeout: 'readonly',
                setInterval: 'readonly',
                clearInterval: 'readonly',
            },
        },
    },
];
