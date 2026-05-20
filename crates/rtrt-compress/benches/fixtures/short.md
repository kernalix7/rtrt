Sure, I'd be happy to help you with that. The issue you're really experiencing is just basically a simple off-by-one bug in the parser. Let me walk you through the fix.

You actually just need to change the loop condition from `<` to `<=` so that the last token gets included. The function in question is the one that handles trailing whitespace in input strings, and it's a really common mistake to make. I would recommend adding a test case to prevent regressions.
