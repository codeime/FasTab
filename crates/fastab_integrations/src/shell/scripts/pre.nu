mkdir ~/.local/bin | ignore

def pathadd [path: string] {
  if not ($env.PATH | any {|it| $it == $path }) {
    $env.PATH | prepend $path
  } else {
    $env.PATH
  }
}

$env.PATH = pathadd $"($env.HOME)/.local/bin"
$env.PATH = pathadd $"($env.HOME)/.local/bin"

if "Q_NEW_SESSION" in $env {
  $env.QTERM_SESSION_ID = null
  $env.Q_TERM = null
  $env.Q_NEW_SESSION = null
}

if "Q_SET_PARENT_CHECK" not-in $env {
  if "Q_PARENT" not-in $env and "Q_SET_PARENT" in $env {
    $env.Q_PARENT = $env.Q_SET_PARENT
    $env.Q_SET_PARENT = null
  }
  $env.Q_SET_PARENT_CHECK = 1
}


let result = (^{{CLI_BINARY_NAME}} _ should-figterm-launch | complete)
$env.SHOULD_QTERM_LAUNCH = $result.exit_code

let should_launch = (
    ("PROCESS_LAUNCHED_BY_Q" not-in $env or ($env.PROCESS_LAUNCHED_BY_Q | str length) == 0)
    and ($env.SHOULD_QTERM_LAUNCH == 0 or
       ($env.SHOULD_QTERM_LAUNCH == 2 and "Q_TERM" not-in $env))
)

if $should_launch {
  let Q_SHELL = ({{CLI_BINARY_NAME}} _ get-shell | complete).stdout
  
  let term_name = $"nu ({{PTY_BINARY_NAME}})"
  let term_path = if "Q_TERM_PATH" in $env {
    $env.Q_TERM_PATH
  } else if ([$env.HOME ".local" "bin" $term_name] | path join | path exists) {
    [$env.HOME ".local" "bin" $term_name] | path join
  } else {
    {{PTY_BINARY_PATH}}
  }

  if ($term_path | path exists) {
    with-env {
      Q_SHELL: $Q_SHELL
    } {
      exec $term_path
    }
  }
}
