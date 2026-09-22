.PHONY: fmt-tilt check-tilt test-tilt
fmt-tilt:
	git diff --name-only -- '*.rs' | xargs rustfmt --edition 2024
	rustfmt --edition 2024 src/platform/pen_tilt.rs
check-tilt:
	cargo check --lib
test-tilt:
	rustc --edition 2024 --test src/platform/pen_tilt.rs -o "$${CARGO_TARGET_DIR:?}/gpui-pen-tilt-tests"
	"$${CARGO_TARGET_DIR:?}/gpui-pen-tilt-tests"
