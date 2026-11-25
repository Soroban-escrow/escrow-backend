.PHONY: check test run doctor release image
check:   ## fmt + clippy + tests (+ audit if installed)
	./scripts/test.sh
test:
	cargo test --workspace --locked
run:     ## API + worker on :3001 (reads .env)
	cargo run --release -- all
doctor:
	cargo run --release -- doctor
release:
	cargo build --release --locked -p escrowd
image:
	docker build -t skillescrow-backend .
