"""The bounded buffer between a session's reader and its caller."""

from __future__ import annotations

import asyncio
import math
from collections import deque
from typing import Generic, TypeVar

T = TypeVar("T")


class Signal:
    """Wakes every task that waits for a change of state."""

    def __init__(self) -> None:
        self._event = asyncio.Event()

    def notify(self) -> None:
        self._event.set()
        self._event = asyncio.Event()

    async def wait(self) -> None:
        await self._event.wait()


class EventBuffer(Generic[T]):
    """Events read from a call and not yet taken, bounded by their size on the wire.

    ``put`` waits while the buffer is full, which is what stops the session
    reading from the transport. One event is always admitted to an empty
    buffer, whatever its size.
    """

    def __init__(self, capacity: int) -> None:
        self._capacity: float = capacity
        self._items: deque[tuple[T, int]] = deque()
        self._used = 0
        self._ended = False
        self._changed = Signal()

    @property
    def used(self) -> int:
        return self._used

    async def put(self, item: T, size: int, patience: float | None = None) -> None:
        """Admit `item` once it fits; an ended buffer drops it.

        Raises ``asyncio.TimeoutError`` when `patience` seconds pass with
        the buffer full and nothing taken from it.
        """
        while self._items and self._used + size > self._capacity and not self._ended:
            await asyncio.wait_for(self._changed.wait(), patience)
        if not self._ended:
            self._items.append((item, size))
            self._used += size
            self._changed.notify()

    async def get(self) -> T | None:
        """The next event, or ``None`` once the buffer has ended and holds nothing."""
        while not self._items:
            if self._ended:
                return None
            await self._changed.wait()
        item, size = self._items.popleft()
        self._used -= size
        self._changed.notify()
        return item

    def unbound(self) -> None:
        """Admit whatever still comes: the call has ended, so the transport holds little more."""
        self._capacity = math.inf
        self._changed.notify()

    def end(self, *, discard: bool = False) -> None:
        """Admit nothing more; with ``discard``, also drop what is held."""
        self._ended = True
        if discard:
            self._items.clear()
            self._used = 0
        self._changed.notify()
