.PHONY: help test engine ui dev build clean smoke-wasm

help:
	@echo "Targets:"
	@echo "  test        - run native engine tests"
	@echo "  engine      - build the engine for the browser (requires wasm-pack)"
	@echo "  ui          - install ui deps and build the production bundle"
	@echo "  dev         - run the ui dev server (vite)"
	@echo "  build       - engine + ui (full production build)"
	@echo "  smoke-wasm  - end-to-end check of the wasm-bindgen surface from node"
	@echo "  clean       - remove build artifacts"

test:
	cargo test

engine:
	cd ui && npm run build:engine

ui:
	cd ui && npm install && npm run build

dev:
	cd ui && npm run dev

build: engine ui

smoke-wasm:
	node scripts/smoke-wasm.mjs

clean:
	cargo clean
	rm -rf ui/node_modules ui/dist ui/src/engine/pkg
