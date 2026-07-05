"""Import this module to auto-instrument every installed provider SDK in a single line:

    import lighttrack.auto   # patches OpenAI / Anthropic / Gemini clients globally

The default client is configured from the environment (``LIGHTTRACK_URL`` / ``LIGHTTRACK_KEY`` /
``LIGHTTRACK_PROJECT``). For explicit control, call ``lighttrack.instrument(lt)`` or
``lighttrack.wrap(client, lt)`` yourself instead of importing this module.
"""

from .instrument import instrument

instrument()
