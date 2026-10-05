.PHONY: install check test lint ci record bench docs-dev docs-build

# scenes to record (default: all), e.g. make record SCENES="hero plots"
SCENES ?=
# scenes recorded at once (default: a sixth of the CPU cores)
JOBS ?=

install:
	cargo install --force --path . --locked

check:
	cargo check --all-targets

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

# what CI runs
ci: lint test

# docs clips and stills into docs/public/media
record:
	nix develop .#docs -c nu docs/media/record.nu $(if $(JOBS),-j $(JOBS)) $(SCENES)

# speed comparison with euporie (docs/media/scenes/bench-editors.nu); run it
# on an idle machine, BENCH_RUNS=n for more repetitions
bench:
	nix develop .#docs -c nu docs/media/record.nu -j 1 bench-editors

docs-dev:
	cd docs && nix develop ..#docs -c sh -c '[ -d node_modules ] || npm ci; npm run dev'

docs-build:
	cd docs && nix develop ..#docs -c sh -c '[ -d node_modules ] || npm ci; npm run build'
