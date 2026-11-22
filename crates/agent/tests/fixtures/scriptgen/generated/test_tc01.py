import pytest

from generated.pages.tc01_pages import (
    HomePage,
    LocalLibraryPage,
    MinePage,
    MusicPage,
    PlayerPage,
    RoomPage,
    VoicePartyPage,
)

ROOM_NAME = "AI Auto Test Room"
SONG = "tst_normal_01"
SONG_DURATION = "00:30"


@pytest.mark.case("TC-01")
def test_tc01_add_local_music_and_play(ui, account):
    home = HomePage(ui)
    mine = MinePage(ui)
    party = VoicePartyPage(ui)
    room = RoomPage(ui)
    music = MusicPage(ui)
    library = LocalLibraryPage(ui)
    player = PlayerPage(ui)

    # 1. Open the app and close any popups.
    home.open_app()

    # 2. Go to Mine and record the nickname.
    home.open_mine()
    nickname = mine.read_nickname()

    # 3. Return to the Party tab.
    home.open_party()

    # 4. Tap the plus button in the top-right corner.
    home.tap_create_room()

    # 5. Set the room name and keep the other fields at their defaults.
    party.set_room_name(ROOM_NAME)

    # 6. Start the party.
    party.start_party()

    # 7. Handle the microphone permission prompt if it appears.
    party.handle_mic_permission()

    # 8. Wait to enter the room and close the debug overlay.
    room.wait_entered(ROOM_NAME)

    # 9. More -> Room Tools -> Music.
    room.open_music()
    assert music.title() == MusicPage.TITLE_EMPTY, \
        f"预期1未满足：添加前 Music 页标题应为 Music(0/50)，实际为 {music.title()}"

    # 10-11. Tap Add Music and handle the music access permission if prompted.
    music.tap_add_music()

    # 12. Select the target song in Local Music Library.
    library.select_song(SONG)

    # 13-14. Tap Add(1) and wait to return to the Music page.
    library.confirm_add(1)

    # Expected 1: the title changes to Music(1/50).
    assert ui.has(MusicPage.TITLE_ONE, exact=True), \
        f"预期1未满足：添加后 Music 页标题应变为 Music(1/50)，实际为 {music.title()}"

    # Expected 2: the list shows the song name and duration 00:30, with no artist info.
    row = music.song_row_label(SONG)
    assert row is not None, f"预期2未满足：Music 列表中未出现 {SONG}"
    parts = [p.strip() for p in row.split(" / ")]
    assert SONG_DURATION in parts, f"预期2未满足：{SONG} 的时长应为 {SONG_DURATION}，实际行内容为「{row}」"
    assert parts == [SONG, SONG_DURATION], \
        f"预期2未满足：列表行应只显示歌名和时长、不显示歌手信息，实际行内容为「{row}」"

    # 15. Tap the song in the list.
    music.play_song(SONG)

    # Expected 3: playback starts and the player shows the song name.
    # (Whether sound actually plays cannot be verified from the UI. Playback is inferred from the increasing progress below.)
    assert player.has_song_title(SONG, row), f"预期3未满足：播放器中未显示歌曲名 {SONG}"

    # Expected 4: the top shows "Current player: " plus a nickname matching the one from the Mine page.
    current_player = player.current_player_name()
    assert current_player is not None, "预期4未满足：播放器顶部未展示 \"Current player: \""
    assert current_player == nickname, \
        f"预期4未满足：Current player 昵称为「{current_player}」，与 Mine 页昵称「{nickname}」不一致"

    # Expected 5: progress starts near 0 seconds and increases, and the total duration shows 00:30.
    start_secs, has_total = player.progress(SONG_DURATION, row)
    assert has_total, f"预期5未满足：播放器总时长未显示为 {SONG_DURATION}"
    assert start_secs is not None, "预期5未满足：未读到播放进度"
    assert start_secs <= 5, f"预期5未满足：播放进度应从 0 秒开始，刚开始播放时已为 {start_secs} 秒"

    ui.wait(3)  # Let the progress run for a few seconds before reading it again.
    later_secs, _ = player.progress(SONG_DURATION, row)
    assert later_secs is not None and later_secs > start_secs, \
        f"预期5未满足：播放进度未递增（{start_secs} 秒 -> {later_secs} 秒）"
