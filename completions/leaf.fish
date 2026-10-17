# Completions for leaf. Install: ln -s (pwd)/completions/leaf.fish ~/.config/fish/completions/
complete -c leaf -f
complete -c leaf -n __fish_use_subcommand -a open -d 'Read a PDF or web page'
complete -c leaf -n '__fish_seen_subcommand_from open' -a '(__fish_complete_suffix .pdf)'
