"""TC-06b Page Objects: the oversized (11-minute) file is blocked when selected.

All content-desc strings and fallback coordinates live here. Hafa is a Flutter
app, so locators use content-desc only.
"""


class RoomPage:
    """Voice chat room page (setup has already entered the room, joined a mic seat and turned the mic on)."""

    # Nine-grid "More" icon at the bottom of the room. It has no text, so it is tapped by fixed coordinates.
    MORE_ICON_XY = (295, 1527)
    # Representative text that appears after the More panel opens
    MORE_PANEL_READY = "Room Tools"

    def __init__(self, ui):
        self.ui = ui

    def open_more(self):
        # Bottom nine-grid "More" icon (no content-desc)
        self.ui.tap_xy(*self.MORE_ICON_XY)
        self.ui.wait_text(self.MORE_PANEL_READY, exact=True)
        return MorePanel(self.ui)


class MorePanel:
    """More panel (Interaction Tools / Room Tools / Others)."""

    MUSIC = "Music"
    MUSIC_PAGE_READY = "Add Music"

    def __init__(self, ui):
        self.ui = ui

    def open_music(self):
        self.ui.tap(self.MUSIC)
        self.ui.wait_text(self.MUSIC_PAGE_READY, exact=True)
        return MusicPage(self.ui)


class MusicPage:
    """Room music list page (Music(x/50))."""

    ADD_MUSIC = ("Add Music", (360, 1532))
    LIBRARY_READY = "Local Music Library"

    def __init__(self, ui):
        self.ui = ui

    def open_add_music(self):
        text, xy = self.ADD_MUSIC
        self.ui.tap(text, xy=xy)
        self.ui.wait_text(self.LIBRARY_READY)
        return LocalLibraryPage(self.ui)


class LocalLibraryPage:
    """Local Music Library selection page.

    The row order differs on every run, so rows are located by song name with
    scroll=True. Trajectory coordinates are not used.
    """

    TITLE = "Local Music Library"
    ADD_BUTTON_PATTERN = r"^Add\(\d+\)$"
    DURATION_LIMIT_TOAST = "Music duration cannot exceed 10 minutes"

    def __init__(self, ui):
        self.ui = ui

    def find_song(self, song_name):
        """Scroll until the song row is visible. Returns the element, or None if missing."""
        return self.ui.scroll_to(song_name, exact=False, max_swipes=15)

    def read_song_duration(self, song_name):
        """Read the row text, formatted as 'name / mm:ss'. Returns the duration string, or None."""
        label = self.ui.read(rf"^{song_name} / \d{{2}}:\d{{2}}$")
        if label is None:
            return None
        return label.split(" / ", 1)[1]

    def tap_song(self, song_name):
        """Tap the song row (centre of the element, located by name).

        No fallback coordinates are given, so a reordered list cannot cause a tap on the wrong row.
        """
        self.ui.tap(song_name, exact=False, scroll=True, wait=0.3)

    def wait_duration_limit_toast(self, timeout=5):
        """Wait for the duration-limit Toast. Returns True if it appears."""
        try:
            self.ui.wait_text(self.DURATION_LIMIT_TOAST, timeout=timeout)
            return True
        except AssertionError:
            return False

    def read_add_button(self):
        """Read the bottom button text, e.g. 'Add(0)'."""
        return self.ui.read(self.ADD_BUTTON_PATTERN)
