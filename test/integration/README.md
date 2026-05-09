# T2 Integration tests

Cross-layer tests that exercise multiple classes together with mocks
substituted at hardware or process boundaries.

Required for every PR when a change crosses layers. Use shared mocks from
[`../mocks/`](../mocks/); do not reimplement mocks inline.
