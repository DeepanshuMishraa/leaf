{
  lib,
  rustPlatform,
  pkg-config,
  fontconfig,
}:
rustPlatform.buildRustPackage {
  pname = "leaf";
  version = "0.3.0";
  src = lib.cleanSource ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    outputHashes = {
      "compiler-0.0.0" = "sha256-HzLngWsZZVVNgkmPYvDgEJHSK7hUq7UX5517nhR4IKc=";
    };
  };

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
