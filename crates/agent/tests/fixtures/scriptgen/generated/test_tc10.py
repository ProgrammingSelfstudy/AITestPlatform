import pytest

from generated.pages.tc10_pages import MusicPage, PlayerPage, RoomPage

# 用例数据
SONG_DEL = "tst_normal_del"
SONG_OTHER = "tst_normal_01"
DEL_FILE_PATH = "/sdcard/Music/tst_normal_del.mp3"


def _time_to_sec(t):
    m, s = t.split(":")
    return int(m) * 60 + int(s)


@pytest.mark.case("TC-10")
def test_tc10_local_file_deleted_while_playing(ui, account, human):
    # 前置（已登录、已在语聊房且上麦开麦、库里有两首测试歌）由 conftest 保证

    # 步骤 1：进入语聊房 → More → Room Tools → Music
    music = RoomPage(ui).open_more().open_music()

    count_before = music.song_count()
    assert music.has_song(SONG_DEL), f"前置不满足：Music 列表中没有 {SONG_DEL}"
    assert music.has_song(SONG_OTHER), f"前置不满足：Music 列表中没有 {SONG_OTHER}"
    other_label_before = music.song_label(SONG_OTHER)

    # 步骤 2：点击 tst_normal_del 开始播放，确认进度在递增
    music.play_song(SONG_DEL)
    player = PlayerPage(ui)
    t1 = player.read_time()
    ui.wait(2)  # 等进度走几秒再读数
    t2 = player.read_time()
    if t1 is not None and t2 is not None:
        assert _time_to_sec(t2) > _time_to_sec(t1), f"{SONG_DEL} 点击后播放进度没有递增（{t1} -> {t2}）"
    # 若时间读数没有暴露在 content-desc 中，播放状态只能由截图/人工确认，这里不硬断言

    # 步骤 3、4：请人工删除文件并等待确认（脚本绝不自行用 adb 删除）
    # 预期 1、2（只请求一次人工、说明清楚、确认后流程继续且不重复索要）由这里只调用一次 human 保证
    human(
        f"请趁 {SONG_DEL} 正在播放，在手机文件管理器中删除 {DEL_FILE_PATH}，"
        f"仅删除这个文件，不要操作 Hafa 的播放控件。"
        f"删除完成后切回 Hafa（停留在当前页面），然后在终端确认。"
    )

    # 步骤 5、6：回到 App，观察播放状态
    # 预期 3：正在播放的歌立即暂停，不自动切换下一首
    p1 = player.read_time()
    ui.wait(2)  # 等几秒确认进度不再走
    p2 = player.read_time()
    if p1 is not None and p2 is not None:
        assert p1 == p2, f"删除文件后 {SONG_DEL} 没有暂停，进度仍在走（{p1} -> {p2}）"
    next_lbl = player.next_label()
    if next_lbl is not None:
        assert SONG_OTHER in next_lbl, f"删除文件后自动切到了下一首，Next 不再是 {SONG_OTHER}（当前：{next_lbl}）"
    # 播放/暂停按钮图标、唱臂状态没有文字可读，声音是否停止也无法由界面验证，只能依赖截图

    # 重新进入 Music 列表
    music = player.open_my_music_library()

    # 预期 4：Music 列表中 tst_normal_del 记录被移除
    assert not music.has_song(SONG_DEL), f"文件删除后 Music 列表中仍有 {SONG_DEL} 记录"
    count_after = music.song_count()
    if count_before is not None:
        assert count_after == count_before - 1, (
            f"Music 列表计数不对：删除前 {count_before} 首，删除后应为 {count_before - 1} 首，实际 {count_after}"
        )

    # 预期 5：其他歌曲记录不受影响
    assert music.song_label(SONG_OTHER) == other_label_before, (
        f"其他歌曲记录受到影响：{SONG_OTHER} 删除前为 {other_label_before!r}，删除后为 {music.song_label(SONG_OTHER)!r}"
    )

    # 测试数据 tst_normal_del.mp3 跑完后由 testdata 重新推送恢复，脚本不处理
