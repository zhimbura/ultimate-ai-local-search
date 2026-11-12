import re

_NON_WORD = re.compile(r"[^a-z0-9]+")


def slugify(title: str) -> str:
    """Turns a title into a lowercase URL slug: "Hello, World!" -> "hello-world"."""
    return _NON_WORD.sub("-", title.lower()).strip("-")
