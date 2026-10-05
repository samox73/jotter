# Changelog

Every release of jOtter. The same notes are on each [GitHub release](https://github.com/samox73/jotter/releases).

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

