{
  lib,
  rustPlatform,
  pkg-config,
  fontconfig,
}:
rustPlatform.buildRustPackage {
  pname = "leaf";
  version = "0.2.0";
  src = lib.cleanSourceWith {
    src = ./.;
    filter =
      path: _:
      !(
        lib.baseNameOf path != null
        && lib.hasInfix "site" (lib.baseNameOf path)
        && lib.hasInfix "completions" (lib.baseNameOf path)
      );
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    fontconfig
  ];

  meta = with lib; {
    description = "Read PDFs, markdown files and web articles in the terminal.";
    homepage = "https://github.com/DeepanshuMishraa/leaf";
    license = licenses.mit;
    maintainers = with maintainers; [ kleindotc ];
    platforms = platforms.unix;
    mainProgram = "leaf";
  };
}
