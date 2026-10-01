export install_dir := env("OH_FX_INSTALL_DIR", home_directory() / ".local/bin")
export target_dir := env("CARGO_TARGET_DIR", justfile_directory() / "target")

[no-exit-message]
[positional-arguments]
run *args:
    @cargo run --quiet --release --locked -p oh-fx -- "$@"

build:
    cargo build --release --locked -p oh-fx

install: build
    mkdir -p "$install_dir"
    install -m 755 "$target_dir/release/oh-fx" "$install_dir/.oh-fx-install"
    mv -f "$install_dir/.oh-fx-install" "$install_dir/oh-fx"
    ln -sf oh-fx "$install_dir/ofx"
    @echo "installed oh-fx $("$install_dir/oh-fx" --version) to $install_dir/oh-fx"
