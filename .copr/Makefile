# Copr "make srpm" entry point: called as
#   make -f .copr/Makefile srpm outdir=<dir> spec=<spec>
# Runs with network access, so this is where crate dependencies are fetched.
# Release = number of commits, so every pushed commit is a dnf update.

top     := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/..)
# Copr's checkout is owned by another user; allow it for this call only
git     := git -c safe.directory=$(top) -C $(top)
outdir  ?= $(top)/rpmbuild
spec    ?= $(top)/agxtop.spec
version := $(shell sed -n 's/^version = "\(.*\)"/\1/p' $(top)/Cargo.toml | head -1)
specver := $(shell sed -n 's/^Version: *//p' $(spec))

srpm:
	@test "$(version)" = "$(specver)" || { echo "Cargo.toml $(version) != spec $(specver)"; exit 1; }
	command -v cargo >/dev/null || dnf -y install cargo
	command -v git >/dev/null || dnf -y install git
	mkdir -p $(outdir)
	$(git) archive --prefix=agxtop-$(version)/ -o $(outdir)/agxtop-$(version).tar.gz HEAD
	cd $(top) && cargo vendor --locked --versioned-dirs $(outdir)/vendor >/dev/null
	tar -C $(outdir) -czf $(outdir)/agxtop-$(version)-vendor.tar.gz vendor
	sed "s/^Release: .*/Release:        $$($(git) rev-list --count HEAD)%{?dist}/" $(spec) > $(outdir)/agxtop.spec
	rpmbuild -bs $(outdir)/agxtop.spec \
	    --define "_sourcedir $(outdir)" --define "_srcrpmdir $(outdir)"

.PHONY: srpm
