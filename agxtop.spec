# Built on Copr from .copr/Makefile, which bundles the crate dependencies
# (cargo vendor) so the build itself runs offline.
%global debug_package %{nil}

Name:           agxtop
Version:        0.1.0
Release:        1%{?dist}
Summary:        nvtop-style GPU monitor for Asahi Linux (Apple Silicon)

# agxtop is GPL-3.0-or-later; the rest covers the bundled Rust crates
License:        GPL-3.0-or-later AND MIT AND Apache-2.0 AND (Apache-2.0 WITH LLVM-exception) AND Unicode-DFS-2016 AND Unicode-3.0 AND Zlib AND WTFPL
URL:            https://github.com/luca-viola/agxtop
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz

# Apple Silicon only
ExclusiveArch:  aarch64

BuildRequires:  cargo >= 1.85
BuildRequires:  rust >= 1.85

%description
agxtop shows GPU load, per-process GPU usage and GPU memory on Asahi Linux,
where the GPU driver has no performance counters. It reads the kernel's
gpu_scheduler tracepoints through perf_event_open, and the GPU layout through
the Asahi DRM driver. It is installed with the cap_perfmon and
cap_dac_read_search capabilities so it runs without sudo; both are dropped
right after startup.

%prep
%autosetup
tar --no-same-owner -xzf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'CFG'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CFG

%build
cargo build --release --frozen --offline

%install
install -Dm755 target/release/agxtop %{buildroot}%{_bindir}/agxtop

%check
cargo test --release --frozen --offline

%files
%license LICENSE
%doc README.md
%caps(cap_perfmon,cap_dac_read_search=ep) %{_bindir}/agxtop

%changelog
* Sat Nov 21 2026 luca-viola <25868012+luca-viola@users.noreply.github.com> - 0.1.0-1
- First package
