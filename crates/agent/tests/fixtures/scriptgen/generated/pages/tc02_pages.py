"""TC-02 Page Object：已添加歌曲在本地库显示 Added 且不可重复添加。

所有 content-desc 文案和兜底坐标都集中在这里。
Hafa 是 Flutter 应用，只按 content-desc 定位，坐标只用于兜底。
"""


class RoomPage:
    """语聊房主页面（底部工具栏）。"""

    # 房间底部 More 九宫格图标：没有任何文字，只能用固定坐标点击
    MORE_ICON_XY = (295, 1527)
    # 打开 More 面板后出现的代表性文字
    MORE_PANEL_MARK = "Room Tools"

    def __init__(self, ui):
        self.ui = ui

    def open_more(self):
        # 语音调试面板有全屏遮罩，会挡住下面的控件，先关掉
        if self.ui.debug_overlay_up():
            self.ui.dismiss_debug_overlay()
        self.ui.tap_xy(*self.MORE_ICON_XY)
        self.ui.wait_text(self.MORE_PANEL_MARK)
        return MorePanel(self.ui)


class MorePanel:
    """More 面板（Interaction Tools / Room Tools / Others）。"""

    MUSIC = ("Music", None)
    # 进入音乐页后出现的代表性文字
    MUSIC_PAGE_MARK = "Add Music"

    def __init__(self, ui):
        self.ui = ui

    def open_music(self):
        text, xy = self.MUSIC
        self.ui.tap(text, xy=xy, exact=True)
        self.ui.wait_text(self.MUSIC_PAGE_MARK)
        return MusicPage(self.ui)


class MusicPage:
    """房间音乐列表页 Music(n/50)。"""

    TITLE_PREFIX = "Music("
    ADD_MUSIC = ("Add Music", (355, 1530))
    # 进入本地库后出现的代表性文字
    LOCAL_LIBRARY_MARK = "Local Music Library"

    def __init__(self, ui):
        self.ui = ui

    def has_song(self, song: str) -> bool:
        # 行的 content-desc 形如 "tst_normal_01 / 00:30"
        return self.ui.read(rf"^{song} / ") is not None

    def open_add_music(self):
        text, xy = self.ADD_MUSIC
        self.ui.tap(text, xy=xy, exact=True)
        self.ui.wait_text(self.LOCAL_LIBRARY_MARK)
        return LocalLibraryPage(self.ui)


class LocalLibraryPage:
    """Local Music Library(n) 页面。列表顺序每次运行都不同，只能按歌名定位。"""

    TITLE = "Local Music Library"
    ADDED_MARK = "Added"
    ADD_BUTTON = ("Add(0)", None)
    SELECT_ALL = "Select all"

    def __init__(self, ui):
        self.ui = ui

    def is_open(self) -> bool:
        return self.ui.has(self.TITLE)

    def locate_song(self, song: str):
        """滚动直到该歌曲行完整可见，返回元素（找不到为 None）。"""
        return self.ui.scroll_to(song, exact=False)

    def song_row_label(self, song: str):
        """返回该行完整文案，如 "tst_normal_01 / 00:30 / Added"。"""
        return self.ui.read(rf"^{song} / ")

    def song_row_clickable(self, song: str) -> bool:
        return self.ui.find(song, exact=False, clickable=True) is not None

    def tap_song(self, song: str):
        # 已添加的行是只读的，点击后页面不应有变化，因此无新文字可等；
        # 只等待该行文案依旧存在
        self.ui.tap(song, exact=False, scroll=True)
        self.ui.wait_text(song)

    def add_button_present(self) -> bool:
        return self.ui.has(self.ADD_BUTTON[0], exact=True)

    def add_button_clickable(self) -> bool:
        return self.ui.find(self.ADD_BUTTON[0], exact=True, clickable=True) is not None

    def tap_add_button(self):
        text, xy = self.ADD_BUTTON
        self.ui.tap(text, xy=xy, exact=True)
        # 按钮置灰，点击后应仍停留在本地库页面
        self.ui.wait_text(self.TITLE)
