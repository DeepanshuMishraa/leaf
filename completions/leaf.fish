# Completions for leaf. Install: ln -s (pwd)/completions/leaf.fish ~/.config/fish/completions/
complete -c leaf -f
complete -c leaf -n __fish_use_subcommand -a open -d 'Read a PDF, markdown file or web page'
complete -c leaf -n '__fish_seen_subcommand_from open' -a '(__fish_complete_suffix .pdf .md .markdown)'
complete -c leaf -n __fish_use_subcommand -a diff -d 'Review a diff'
complete -c leaf -n '__fish_seen_subcommand_from diff' -s s -l staged -d 'Staged changes'
complete -c leaf -n '__fish_seen_subcommand_from diff' -s b -l branch -d 'This branch against its base branch'
complete -c leaf -n '__fish_seen_subcommand_from diff' -l base -x -a '(__fish_git_branches)' -d 'Compare against a base'
complete -c leaf -n '__fish_seen_subcommand_from diff' -s r -l range -x -d 'Revision or revision range'
complete -c leaf -n '__fish_seen_subcommand_from diff' -l patch -r -a '(__fish_complete_suffix .diff .patch)' -d 'Diff or patch file'
complete -c leaf -n '__fish_seen_subcommand_from diff' -a '(__fish_complete_suffix .diff .patch)' -d 'Diff or patch file'
