# Changelog

Every release of jOtter. The same notes are on each [GitHub release](https://github.com/samox73/jotter/releases).

## 0.6.0 (2026-11-10)

### Features

- Support jupytext percent format and plain python files (#4) ([e9982e3](https://github.com/samox73/jotter/commit/e9982e352bb8371e9b30c4cda9b6a26241b0adaf))

### Bug fixes

- Minimum Rust is 1.90, and openSUSE needs --allow-unsigned-rpm ([1e3f617](https://github.com/samox73/jotter/commit/1e3f6177364fd99a568518778bc6b9995eea7bb3))
- Updating plots no longer flicker or blank in kitty, and each frame is drawn as one synchronized update ([43b73b5](https://github.com/samox73/jotter/commit/43b73b597e16578ce878e033d48f5ad4d5bad740))
- Run editor string through sh (#6) ([f95998a](https://github.com/samox73/jotter/commit/f95998a6b7282814fc8e8e7ea02822e48bd023ee))

### Documentation

- Say that jOtter is built with Claude Code ([10f9770](https://github.com/samox73/jotter/commit/10f9770feae3af7416a47460ced5dc6b2d0e6f7b))

## 0.5.2 (2026-10-22)

### Bug fixes

- Publish the PyPI wheels that v0.5.1 failed to build ([2a42155](https://github.com/samox73/jotter/commit/2a4215527656292adb7f37c7b86eb830ef68ad5e))

## 0.5.1 (2026-10-22)

### Features

- Install from PyPI with `uv tool install jotter-tui` ([ca26f49](https://github.com/samox73/jotter/commit/ca26f491a241d19a964ab1157bb8eb8519ffe47d))

## 0.5.0 (2026-10-21)

### Features

- Jotter without a file opens a scratchpad; :w name.ipynb saves it ([3925161](https://github.com/samox73/jotter/commit/3925161d0ceb5cdb781c8483e2760406aa29ec0e))
- Ctrl+g opens the cell in $EDITOR, as in Claude Code and Codex (was E) ([7d5d6f8](https://github.com/samox73/jotter/commit/7d5d6f81534141bc6242340d72f15a308880315f))

### Bug fixes

- Black text on IPython's yellow traceback highlight ([510abd0](https://github.com/samox73/jotter/commit/510abd0ae4dbaefb5e8efc2704f04f0376b1a418))
- A crash hands the terminal back and keeps the unsaved work ([75202cf](https://github.com/samox73/jotter/commit/75202cf2b93ac6b7e587906008b7725f19c0285f))
- A slow nvim reply no longer loses the nvim backend ([1531e2e](https://github.com/samox73/jotter/commit/1531e2e3bbd82c77e64e0d55ae5b4bb211ab6961))

### Performance

- One redraw per burst of keys, and bracketed paste in one step ([887b32d](https://github.com/samox73/jotter/commit/887b32d4c8f2688e0be51f821b58056a1316def0))
- One terminal round trip less at startup, and a 2 s cap for silent terminals ([d20beda](https://github.com/samox73/jotter/commit/d20beda9aaa860ee12070a00e44905d0811b2c73))

### Documentation

- Konsole, GNOME Terminal and xterm in the terminal gallery, measured ([1624eba](https://github.com/samox73/jotter/commit/1624eba8209f2af57001c5b68df481a028013966))

## 0.4.0 (2026-10-12)

### Features

- Format code cells with an external formatter (f, F, format_on_save) ([a3d7dcd](https://github.com/samox73/jotter/commit/a3d7dcdc4fa06841ae5fb066684c86e780dbbe4b))
- Vim command line (:w, :q, :wq, :x, :5) in a popup ([4729c78](https://github.com/samox73/jotter/commit/4729c7871420c6e3d3a17bc42f41c14bc8ad1b71))
- Vim jump list (Ctrl+o / Ctrl+i) and gg ([0ce10b1](https://github.com/samox73/jotter/commit/0ce10b11c8cbd0348ca82f291ea7efbf01a2076c))
- Select several cells with V and act on them all ([719d1ef](https://github.com/samox73/jotter/commit/719d1eff9d6a19e093f97361f7cd521db3c5ef7b))
- P pastes cells above the selected one ([61e16d6](https://github.com/samox73/jotter/commit/61e16d6f68987bc8c6fc9fcdb8ebb9fb509698ca))
- Scroll the selected cell to the top, middle or bottom with zt, zz, zb; zoom moves to zi ([9271b07](https://github.com/samox73/jotter/commit/9271b074b094eab333133534d595060b0e71efe0))
- Vim marks on cells (ma, 'a, :delm) with a marks sidebar; cell type moves to t ([fad9a5d](https://github.com/samox73/jotter/commit/fad9a5d229b23d84d6d74d8d53671ea8ba85f615))

### Bug fixes

- Render inline math and spans in markdown headings ([9efe9e6](https://github.com/samox73/jotter/commit/9efe9e60c336c1205fd4378ec98c93b52109eb57))

### Performance

- Render only the cells near the screen ([f2c4659](https://github.com/samox73/jotter/commit/f2c4659b55e0bb36981811748ec7a3a86e5e7b3d))

### Documentation

- Portrait otter for the hero and header ([c71ff56](https://github.com/samox73/jotter/commit/c71ff5605daa8d04488ea881854b997397280e3e))
- Compare jOtter with euporie, jpterm, jupyter-console and Neovim plugins ([5e443bf](https://github.com/samox73/jotter/commit/5e443bf07cc1811db2d5db661845aa4fc16433c8))
- Re-record the plots and first-notebook clips for the zi and t keys ([5caf9d1](https://github.com/samox73/jotter/commit/5caf9d10952c7545254b2544c671971ee9afffd1))

## 0.3.0 (2026-10-05)

### Features

- :w, :wq and ZZ in the embedded nvim save the notebook ([209e67c](https://github.com/samox73/jotter/commit/209e67ce4f0905c63eba3a7bc5812b1ffd868678))
- Signature help while typing a call ([41e181b](https://github.com/samox73/jotter/commit/41e181b639bab8a1441b4a081962588910d0e15b))

### Bug fixes

- Close fullscreen plot before surfacing input field ([7eef99d](https://github.com/samox73/jotter/commit/7eef99da9dccb9f8c1de440d546223ddbca608b9))
- Cells no longer hang after completion with ipykernel 7, and closing the terminal doesn't crash jOtter ([92f61df](https://github.com/samox73/jotter/commit/92f61df3ef29fb5116c196c96c6a0496b226ded8))
- Display math is at most max_math_rows tall (default 4) and centred; tall inline math is centred ([c2dbf26](https://github.com/samox73/jotter/commit/c2dbf264d5c5c02d00101789622f21ab342dad0b))
- Keep invalid code visible when highlighting ([c36f603](https://github.com/samox73/jotter/commit/c36f603059ebb2852c0dfe2e2bc2012c38d59c1b))
- Readable signatures in the completion popup ([a99f742](https://github.com/samox73/jotter/commit/a99f7424a84b4ad3eb610da2a1a1cf5f63ad1b53))
- Don't time out opening the first markdown cell in nvim ([d079078](https://github.com/samox73/jotter/commit/d079078f4f05615aa6f7c9f3861e26274a217fcc))

### Performance

- Start nvim in the background and skip its treesitter highlighting ([b952ea2](https://github.com/samox73/jotter/commit/b952ea246e3d61869158504e4f51b153d2e4d21c))

### Documentation

- Smoother typing in clips; Makefile for common commands ([c41f1d9](https://github.com/samox73/jotter/commit/c41f1d956fc61f56245341bb32acbf3d22f1223e))
- What completion knows before cells have run ([7ff4227](https://github.com/samox73/jotter/commit/7ff4227e5f80a40512999623915af840f949c72e))
- Transparent otter in the site header and the landing hero ([a416e9b](https://github.com/samox73/jotter/commit/a416e9b9036e798e43772c6bda3a0a14d865f0b7))

## 0.2.0 (2026-09-27)

### Features

- Show plots in the terminal's colours ([73aedfc](https://github.com/samox73/jotter/commit/73aedfc0ccbe434cb6b483e190e7afddb8c529ab))
- Image sizes in the D debug report ([2c77c50](https://github.com/samox73/jotter/commit/2c77c504f59c2d7e898026e7085727699e59a68f))
- Report the terminal's keyboard protocol ([593e290](https://github.com/samox73/jotter/commit/593e290474e6ad9318583d0944d1629b14522380))

### Bug fixes

- Questions get the whole status line ([995c473](https://github.com/samox73/jotter/commit/995c473e0968ba43a4937e1cc9108b98be1c04f2))
- Sharp inline and display math ([6237454](https://github.com/samox73/jotter/commit/623745406ce25c4801ca06ec9aaec418d3572bf3))

### Documentation

- Starlight documentation site with generated reference ([4c173c8](https://github.com/samox73/jotter/commit/4c173c8e882f78d161b18f4461b08328bd846aa1))
- Recording rig for feature clips, first three clips ([8811203](https://github.com/samox73/jotter/commit/8811203cdf3e348cba01b6c36450e21f024f21e4))
- Recolouring in the outputs guide; demo notebooks without styling ([2f40eb4](https://github.com/samox73/jotter/commit/2f40eb49ad881379940d8e7536673db8130eae84))
- Slower hero clip ([71fdb1e](https://github.com/samox73/jotter/commit/71fdb1e05a15b7ad2beda2cb52420b954cb28b97))
- The remaining feature clips ([850b26d](https://github.com/samox73/jotter/commit/850b26df5b3fa4c247556978984ae24b5f300a3b))
- Record scenes in parallel ([4980551](https://github.com/samox73/jotter/commit/498055142189220246b06d39ac03d000a09c59a9))
- Inline math sizing in the math guide; slash fraction in the demo ([3539ea0](https://github.com/samox73/jotter/commit/3539ea0c27581db94843dbaed66a9d1ff061f3a0))
- Theme and terminal galleries, verified compatibility table ([a0ea7a9](https://github.com/samox73/jotter/commit/a0ea7a95ba0fb23d020ee7af0243f39c4f170ab8))
- Changelog from git-cliff, short README ([a5e5182](https://github.com/samox73/jotter/commit/a5e51826a4f5de6d40884cce2031ab4dfb076346))
- Record clips locally and commit them ([5083c4d](https://github.com/samox73/jotter/commit/5083c4db3f0884ab08c3e318880c18cc8721d120))

## 0.1.0 (2026-09-25)

First release.

### Features

- Read-only execution ([8cf1817](https://github.com/samox73/jotter/commit/8cf181721d8e85cb2c77750f35c62793d99fe2c3))
- Add latex stuff and refine inline math ([e9c5f58](https://github.com/samox73/jotter/commit/e9c5f581a6d34a2a60a813ecf0308ff9351bddd3))
- Production hardening — output pipeline, daily-driver features, polish ([866f0a4](https://github.com/samox73/jotter/commit/866f0a41fa7fccd23db824762fba408041178276))
- Embedded native nvim ([424ba98](https://github.com/samox73/jotter/commit/424ba982e184eb783afcc45ab5b7c1926056fac6))
- Completion, soft wrapping, cell ops, and nbformat/kernel fixes ([5407fa9](https://github.com/samox73/jotter/commit/5407fa947d6cc3ed04fa79f882cdfea54bfa6d25))

### Bug fixes

- Multiple graphics bugs ([620835b](https://github.com/samox73/jotter/commit/620835bf6cf4ba7420b4d542cbe512d9a6542b4d))
- Bugs ([03c1f6f](https://github.com/samox73/jotter/commit/03c1f6f2d930d6eae414c5169c69742ec0425fb7))
- Minor improvements ([697f932](https://github.com/samox73/jotter/commit/697f9329ee4ccf7bf4d729dd7b56db72fc9ef488))
- Don't write "outputs": null into markdown/raw cells ([a3bc853](https://github.com/samox73/jotter/commit/a3bc8537fdcf0d8712f42893203db07ce04ad8a3))

