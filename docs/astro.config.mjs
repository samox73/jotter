// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';

const repo = 'https://github.com/samox73/jotter';

export default defineConfig({
  site: 'https://samox73.github.io',
  base: '/jotter',
  trailingSlash: 'always',
  // screenshots and the mascot live in the repo's top-level assets/
  vite: { server: { fs: { allow: ['..'] } } },
  integrations: [
    starlight({
      title: 'jOtter',
      description: 'A fast Jupyter notebook TUI. Open, edit and run .ipynb notebooks in your terminal.',
      logo: { src: './src/assets/icon.png', alt: 'jOtter' },
      favicon: '/favicon.png',
      social: [{ icon: 'github', label: 'GitHub', href: repo }],
      editLink: { baseUrl: `${repo}/edit/main/docs/` },
      lastUpdated: true,
      customCss: ['./src/styles/theme.css'],
      plugins: [starlightLinksValidator()],
      sidebar: [
        {
          label: 'Getting started',
          items: [
            'getting-started/installation',
            'getting-started/kernels',
            'getting-started/first-notebook',
            'getting-started/terminal-setup',
          ],
        },
        {
          label: 'Guides',
          items: [
            'guides/editing',
            'guides/running-code',
            'guides/outputs',
            'guides/math',
            'guides/markdown',
            'guides/completion',
            'guides/navigation',
            'guides/cell-operations',
            'guides/mouse',
            'guides/external-editor',
            'guides/neovim',
            'guides/data-safety',
            'guides/git',
          ],
        },
        {
          label: 'Reference',
          items: [
            'reference/keybindings',
            'reference/configuration',
            'reference/themes',
            'reference/cli',
            'reference/files',
            'reference/nbformat',
            'reference/terminals',
          ],
        },
        {
          label: 'Help',
          items: [
            'help/troubleshooting',
            'help/faq',
            'help/comparison',
            'help/limitations',
            'help/reporting-bugs',
          ],
        },
        {
          label: 'Project',
          items: [
            'project/contributing',
            'project/architecture',
            'project/changelog',
          ],
        },
      ],
    }),
  ],
});
