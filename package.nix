{
  lib,
  stdenv,
  rustPlatform,
  installShellFiles,
  pkg-config,
  fontconfig,
  alsa-lib,
  ...
}:
let
  manifest = lib.importTOML ./Cargo.toml;
  program = (lib.head manifest.bin).name;
in
rustPlatform.buildRustPackage (finalAttrs: {
  pname = manifest.package.name;
  version = manifest.package.version;
  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  doCheck = false;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
    installShellFiles
  ];

  buildInputs = [
    fontconfig
  ]
  ++ lib.optionals stdenv.hostPlatform.isLinux [ alsa-lib ];

  postInstall = ''
    installShellCompletion           \
        --cmd ${program}             \
        --fish completions/leaf.fish \
        --zsh completions/_leaf
  '';

  meta = with lib; {
    description = manifest.package.description;
    homepage = manifest.package.repository;
    license = licenses.mit;
    platforms = platforms.unix;
    mainProgram = program;
  };
})
