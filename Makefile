.PHONY: help
help: ## ヘルプを表示
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

# BINDIRはinstall-korylusfactoryが実行ファイルを置くディレクトリ。
BINDIR ?= $(HOME)/.local/bin

# ビルドとインストールで、Cargoの出力先を揃える。
CARGO_TARGET_DIR ?= target

# Cargoの設定や環境変数でビルドのターゲットを変えていても、ホスト向けの実行ファイルを置く。
# `=` で定義し、install-korylusfactoryを実行するときだけrustcを呼ぶ (Rustを入れていない環境で、ほかのMakeのターゲットにrustcが見つからないエラーを出さないため)。
RUST_HOST = $(shell rustc -vV | sed -n 's/^host: //p')

.PHONY: install-korylusfactory
install-korylusfactory: ## korylusfactoryをビルドしてBINDIR (既定は~/.local/bin) に置く
	cargo build --release --locked -p korylusfactory-cli --target-dir "$(CARGO_TARGET_DIR)" --target "$(RUST_HOST)"
	mkdir -p "$(BINDIR)"
	install -m 755 "$(CARGO_TARGET_DIR)/$(RUST_HOST)/release/korylusfactory" "$(BINDIR)/korylusfactory"

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
