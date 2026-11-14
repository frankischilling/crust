# Stateful counter plugin

Saves a counter and note in the plugin's data directory. It uses:

- `c2.plugin_data_dir()` for persistence
- `c2.register_command()` for a small stateful tool
- `c2.register_callback()` for completions
- basic file I/O for saved plugin state

Try these commands:

- `/counter`
- `/counter inc`
- `/counter note remember this`
- `/counter reset`
