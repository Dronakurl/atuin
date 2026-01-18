# Run atuin sync on Fish startup if fish_sync.sync_on_startup is enabled
function __atuin_sync_on_startup
    # Check if fish_sync.sync_on_startup is enabled using atuin (proper config parsing)
    if not atuin sync --should-fish-sync >/dev/null 2>&1
        return
    end

    # Set up PID file to prevent duplicate syncs
    set -l pid_file "$XDG_STATE_HOME/atuin/sync.pid"
    if test -z "$pid_file" -o ! -d (dirname "$pid_file" 2>/dev/null)
        set pid_file "$HOME/.local/state/atuin/sync.pid"
    end

    # Check if sync is already running
    if test -f "$pid_file"
        set -l pid (cat "$pid_file")
        if kill -0 "$pid" 2>/dev/null
            return  # Sync already running
        end
    end

    # Ensure directory exists
    mkdir -pm 700 (dirname "$pid_file")

    # Run sync in background with PID tracking
    ATUIN_SYNC_PID_FILE="$pid_file" atuin sync >/dev/null 2>&1 &
    disown
end

# Run sync on Fish init
__atuin_sync_on_startup

set -gx ATUIN_SESSION (atuin uuid)
set --erase ATUIN_HISTORY_ID

function _atuin_preexec --on-event fish_preexec
    if not test -n "$fish_private_mode"
        set -g ATUIN_HISTORY_ID (atuin history start -- "$argv[1]")
    end
end

function _atuin_postexec --on-event fish_postexec
    set -l s $status

    if test -n "$ATUIN_HISTORY_ID"
        ATUIN_LOG=error atuin history end --exit $s -- $ATUIN_HISTORY_ID &>/dev/null &
        disown
    end

    set --erase ATUIN_HISTORY_ID
end

function _atuin_search
    set -l keymap_mode
    switch $fish_key_bindings
        case fish_vi_key_bindings
            switch $fish_bind_mode
                case default
                    set keymap_mode vim-normal
                case insert
                    set keymap_mode vim-insert
            end
        case '*'
            set keymap_mode emacs
    end

    # In fish 3.4 and above we can use `"$(some command)"` to keep multiple lines separate;
    # but to support fish 3.3 we need to use `(some command | string collect)`.
    # https://fishshell.com/docs/current/relnotes.html#id24 (fish 3.4 "Notable improvements and fixes")
    set -l ATUIN_H (ATUIN_SHELL=fish ATUIN_LOG=error ATUIN_QUERY=(commandline -b) atuin search --keymap-mode=$keymap_mode $argv -i 3>&1 1>&2 2>&3 | string collect)

    if test -n "$ATUIN_H"
        if string match --quiet '__atuin_accept__:*' "$ATUIN_H"
          set -l ATUIN_HIST (string replace "__atuin_accept__:" "" -- "$ATUIN_H" | string collect)
          commandline -r "$ATUIN_HIST"
          commandline -f repaint
          commandline -f execute
          return
        else
          commandline -r "$ATUIN_H"
        end
    end

    commandline -f repaint
end

function _atuin_bind_up
    # Fallback to fish's builtin up-or-search if we're in search or paging mode
    if commandline --search-mode; or commandline --paging-mode
        up-or-search
        return
    end

    # Only invoke atuin if we're on the top line of the command
    set -l lineno (commandline --line)

    switch $lineno
        case 1
            _atuin_search --shell-up-key-binding
        case '*'
            up-or-search
    end
end
