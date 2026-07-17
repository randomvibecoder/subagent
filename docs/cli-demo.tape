Output docs/assets/cli-demo.gif

Set Shell "bash"
Set Width 1440
Set Height 900
Set FontSize 16
Set FontFamily "DejaVu Sans Mono"
Set TypingSpeed 28ms
Set Framerate 20
Set Theme { "name": "Subagent", "black": "#000000", "red": "#ff6b6b", "green": "#5ee08b", "yellow": "#f4d35e", "blue": "#70a5eb", "magenta": "#c099ff", "cyan": "#55d6be", "white": "#ffffff", "brightBlack": "#777777", "brightRed": "#ff8787", "brightGreen": "#7bf1a8", "brightYellow": "#ffe082", "brightBlue": "#91b9ff", "brightMagenta": "#d2b3ff", "brightCyan": "#79ead4", "brightWhite": "#ffffff", "background": "#000000", "foreground": "#ffffff", "selection": "#333333", "cursor": "#ffffff" }

Hide
Type "bash scripts/demo-session.sh setup >/tmp/subagent-demo-root && tmux attach-session -t subagent-demo"
Enter
Sleep 1s
Show

Type "subagent daemon start"
Enter
Sleep 1s

Type "subagent agents spawn --name auth-review --dir /tmp/subagent-cli-demo/auth-project --mode readonly --message DEMO_AUTH | tee /tmp/subagent-cli-demo/auth.json"
Enter
Sleep 1s

Type "subagent agents spawn --name test-suite --dir /tmp/subagent-cli-demo/test-project --mode readonly --message DEMO_TEST | tee /tmp/subagent-cli-demo/test.json"
Enter
Sleep 1s

Type "subagent agents list --status working"
Enter
Sleep 9s

Type "subagent inbox list --priority 2"
Enter
Sleep 4s

Hide
Ctrl+B
Type "d"
Sleep 500ms
Type "bash scripts/demo-session.sh cleanup"
Enter
Sleep 1s
