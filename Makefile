VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
PROFILE := release
EXE := target/$(PROFILE)/ah
prefix := /usr/local
bindir := $(prefix)/bin
datadir := $(prefix)/share
mandir := $(datadir)/man/man1
exe_name := ah

.PHONY: build install test lint fmt completions man demo release clean

build: $(EXE)

$(EXE): Cargo.toml src/**/*.rs
	cargo build --profile $(PROFILE)

test:
	cargo test

lint:
	cargo clippy -- -D warnings

fmt:
	cargo fmt --check

completions: $(EXE)
	@mkdir -p completions
	$(EXE) completion bash > completions/$(exe_name).bash
	$(EXE) completion fish > completions/$(exe_name).fish
	$(EXE) completion zsh  > completions/_$(exe_name)

man: $(EXE)
	$(EXE) man --out-dir man

install: $(EXE) completions man
	install -Dm755 $(EXE) $(DESTDIR)$(bindir)/$(exe_name)
	install -Dm644 completions/$(exe_name).bash $(DESTDIR)$(datadir)/bash-completion/completions/$(exe_name)
	install -Dm644 completions/$(exe_name).fish $(DESTDIR)$(datadir)/fish/vendor_completions.d/$(exe_name).fish
	install -Dm644 completions/_$(exe_name) $(DESTDIR)$(datadir)/zsh/site-functions/_$(exe_name)
	for f in man/*.1; do install -Dm644 "$$f" "$(DESTDIR)$(mandir)/$$(basename $$f)"; done

demo: $(EXE)
	cargo install --path . --force
	bash demo/setup.sh
	vhs demo/demo-pre.tape
	bash demo/setup.sh
	vhs demo/demo.tape

release:
	@echo "Tagging v$(VERSION) and pushing..."
	git tag "v$(VERSION)"
	git push origin "v$(VERSION)"
	@echo "GitHub Actions will build and create the release."

clean:
	cargo clean
	rm -rf completions man

-include Makefile.local
