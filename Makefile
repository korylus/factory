.PHONY: help
help: ## ヘルプを表示
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

# BINDIRはinstall-korylusfactoryとinstall-korylusfactory-webが実行ファイルを置くディレクトリ。
BINDIR ?= $(HOME)/.local/bin

# ビルドとインストールで、Cargoの出力先を揃える。
CARGO_TARGET_DIR ?= target

# Cargoの設定や環境変数でビルドのターゲットを変えていても、ホスト向けの実行ファイルを置く。
# `=` で定義し、インストールのターゲットを実行するときだけrustcを呼ぶ (Rustを入れていない環境で、ほかのMakeのターゲットにrustcが見つからないエラーを出さないため)。
RUST_HOST = $(shell rustc -vV | sed -n 's/^host: //p')

.PHONY: install-korylusfactory
install-korylusfactory: ## korylusfactoryをビルドしてBINDIR (既定は~/.local/bin) に置く
	cargo build --release --locked -p korylusfactory-cli --target-dir "$(CARGO_TARGET_DIR)" --target "$(RUST_HOST)"
	mkdir -p "$(BINDIR)"
	install -m 755 "$(CARGO_TARGET_DIR)/$(RUST_HOST)/release/korylusfactory" "$(BINDIR)/korylusfactory"

.PHONY: install-korylusfactory-web
install-korylusfactory-web: ## 管制のウェブアプリをビルドしてBINDIR (既定は~/.local/bin) に置く
	cargo build --release --locked -p korylusfactory-web --target-dir "$(CARGO_TARGET_DIR)" --target "$(RUST_HOST)"
	mkdir -p "$(BINDIR)"
	install -m 755 "$(CARGO_TARGET_DIR)/$(RUST_HOST)/release/korylusfactory-web" "$(BINDIR)/korylusfactory-web"

.PHONY: dev-web
dev-web: ## ウェブアプリを開発用の設定 (開発用の身元で人間とみなす) で起動
	mkdir -p tmp
	cargo run --locked -p korylusfactory-web -- --config crates/korylusfactory-web/config.dev.json

# ウェブアプリのクレート。CSSとJavaScriptをここの static/ に作り、実行ファイルに埋め込む。
WEB_DIR = crates/korylusfactory-web

.PHONY: web-assets
web-assets: ## ウェブアプリのCSS (トークンを含む) とJavaScriptを static/ に作る
	node $(WEB_DIR)/scripts/generate-tokens.mjs
	@set -eu; \
		css_tmp=$$(mktemp "$(WEB_DIR)/static/.app.css.XXXXXX"); \
		trap 'rm -f "$$css_tmp"' EXIT; \
		pnpm exec tailwindcss --input $(WEB_DIR)/assets/app.css --output "$$css_tmp" --minify; \
		{ \
			printf '\n/*! Basecoat CSS\n'; \
			cat node_modules/basecoat-css/LICENSE.md; \
			printf '\nTailwind CSS\n'; \
			cat node_modules/tailwindcss/LICENSE; \
			printf '\n*/\n'; \
		} >> "$$css_tmp"; \
		chmod 644 "$$css_tmp"; \
		mv "$$css_tmp" $(WEB_DIR)/static/app.css
	cp node_modules/htmx.org/dist/htmx.min.js $(WEB_DIR)/static/htmx.min.js
	cp node_modules/htmx.org/dist/ext/hx-sse.min.js $(WEB_DIR)/static/hx-sse.min.js

.PHONY: web-assets-check
web-assets-check: web-assets ## 作り直したCSSとJavaScriptが、リポジトリに入れたものと同じかを確かめる
	@if [ -n "$$(git status --porcelain -- $(WEB_DIR)/assets $(WEB_DIR)/static)" ]; then \
		git status --short -- $(WEB_DIR)/assets $(WEB_DIR)/static; \
		echo "ウェブアプリのCSSかJavaScriptが、テンプレートやトークンと食い違っています。make web-assets で作り直してコミットしてください"; \
		exit 1; \
	fi

.PHONY: fmt
fmt: ## コード・ドキュメントをフォーマット (rustfmt + Oxfmt)
	cargo fmt --all
	pnpm fmt

.PHONY: fmt-check
fmt-check: ## フォーマットチェック (rustfmt + Oxfmt)
	cargo fmt --all --check
	pnpm fmt:check

.PHONY: lint
lint: ## clippyを実行
	cargo clippy --workspace --all-targets --locked -- -D warnings

.PHONY: test
test: ## テストを実行
	cargo test --workspace --locked
	node --test $(WEB_DIR)/scripts/
