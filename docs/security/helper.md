# The elevated helper: security model

`bytescape-helper` is the one part of Bytescape that runs with administrator rights. This page says who it defends against, how it is started, what it promises, and which test shows each promise. You can read it next to the code and check one against the other.

It is written for people who review or package the helper. If you only want to know what the helper does, read [SECURITY.md](../../SECURITY.md).

## What the helper does

The helper does one job per run and then exits:

- **A full scan.** It reads a drive as the administrator and streams the result to the interface, so the map can show folders the user's account cannot open. It reads the drive's own metadata where it has a reader for the file system (ext4, btrfs and XFS on Linux, NTFS on Windows) and walks the folders otherwise.
- **An elevated delete.** It permanently deletes the items the interface lists, and only those, after judging each one itself.

It never starts another program, never opens a network connection, and never writes anything except the deletes it was asked for. It does not move items to the Trash or Recycle Bin: on Linux the superuser's Trash is not the user's, so an elevated delete is permanent, and the interface says so before it asks.

## Who we defend against

| Attacker | What they can do | What we want |
|---|---|---|
| A program running as the same user, trying to use the helper to gain rights | Start the helper, speak the protocol, race the file system | The helper does nothing the user could not do as an administrator, and refuses what no one should do (operating system folders). A program cannot talk to a helper that the interface started. |
| Another user on the same machine | Open the helper's pipe or start their own helper | Cannot reach the interface's helper. A helper they start serves only the user who authenticates for it. |
| A bug in the interface | Send wrong, relative, or dressed-up paths | The helper judges every path itself, as if the interface were hostile. |
| Hostile disk contents | Names and structures made to confuse a reader | The scan treats everything it reads as untrusted. Links are never followed. |
| Someone starting the helper by hand | Wrong arguments, wrong pipes, no user | The helper refuses before doing any work. |

Out of scope: an attacker who already has administrator rights, a compromised kernel, and the user who authenticates and then chooses to approve what the dialog shows.

## Entry points

Everything the helper reads from outside arrives in one of these ways. Each is parsed by code with no panicking shortcuts and no unchecked arithmetic (the crate lints deny them).

| Entry point | Who controls it | Checked by |
|---|---|---|
| Command line | the program that starts it | a fixed schema (`args.rs`): mode, protocol version, launch value, and on Windows the pipe name and parent process id |
| `PKEXEC_UID` (Linux) | `pkexec`, which cleans the environment | a plain number other than zero that names the parent's owner |
| Standard input and output (Linux) | the interface | must be anonymous pipes |
| The named pipe (Windows) | the interface | exact name shape, server process id, server program |
| Protocol frames | the interface | `bs-ipc`: length limit, version, kind, field limits, counts checked against the data |
| Paths in a delete request | the interface | the judge (`delete.rs`) |
| The volume and the user's places in a scan request | the interface | absolute, plain, a real folder; places only from the table's variable names, absolute |
| Disk contents | whoever wrote the disk | the walker and the raw reader, which treat all of it as hostile |

The helper trusts no other environment variable, and reads nothing from its own working folder. It never changes its own environment either: the user's places for a scan travel as data and are handed to the scan, not set as variables.

## How it is started

### Linux

The interface runs `pkexec /usr/libexec/bytescape/bytescape-helper <scan|delete> --protocol 1 --nonce <32 hex digits>` with pipes on standard input and output. `pkexec` asks the user to authenticate, cleans the environment, sets `PKEXEC_UID`, and runs the helper in its own process, so the helper's parent is the interface.

- The channel is the proof of who is on the other end: only the interface holds the other ends of the pipes.
- The helper still checks that `PKEXEC_UID` is a non-zero number, that its parent process belongs to that user, and that standard input and output are pipes. A helper started from a terminal, a file or a socket refuses.
- The polkit policy (`packaging/linux/org.recursiveai.bytescape.helper.policy`) has two actions, chosen by the first argument: `org.recursiveai.bytescape.helper.scan` (the authorization may be kept for the session) and `org.recursiveai.bytescape.helper.delete` (asked every time). A run serves only the kind of request its first argument names, so a kept scan authorization cannot be spent on a delete.
- A helper that is not at the installed path (a portable copy, or the one inside a classic snap) matches neither action, and `pkexec` shows its generic prompt.
- `pkexec` itself is started from `/usr/bin/pkexec` (or `/bin/pkexec`), never found on the user's `PATH`: a program of that name in a folder the user can write to would be handed the administrator's password. It has to be the administrator's set-user-id program, in folders only the administrator can change, which is the same check the helper's path gets. Otherwise the interface says it cannot ask for administrator rights, and starts nothing.
- Which helper the interface starts is decided by `Elevation::system()`:
  - Inside an **AppImage** (`$APPIMAGE` is set) there is no helper to start. An AppImage is mounted for the account that started it, so the administrator cannot enter it. The interface says so (`Unavailable::AppImage`) where the offer of a full scan would be.
  - Inside a **snap**, the helper in the snap is used only if the snap's `meta/snap.yaml` says `confinement: classic`; a strict snap cannot ask for administrator rights (`Unavailable::SnapConfined`).
  - Otherwise it is `/usr/libexec/bytescape/bytescape-helper`.
  - `$SNAP` and `$APPIMAGE` are the user's to set. Setting them can only turn elevation off, or point it at a file that is then checked like any other: the path is resolved first, and the helper it leads to and every folder from `/` down to that file must be owned by the administrator and not writable by the group or others. The resolved path is what the interface keeps, checks again just before starting, and gives to `pkexec`, so the program that starts is the file that was checked, even if a link on the way is changed in between. A folder the user made, with a `snap.yaml` that says classic, fails that check if the helper is a file in it. If its helper is a link to a file that does pass, that file is what runs, and the user's folder is not on its path.

### Windows

The interface creates the pipe `\\.\pipe\bytescape-<128 random bits>` first:

- as its first and only instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`), so no other process can take the name;
- with an access list that admits only the current user (`D:P(A;;GA;;;<user SID>)`, with no inherited entries), and `PIPE_REJECT_REMOTE_CLIENTS`;
- for overlapped I/O, so a read that is waiting never holds up a write on the same pipe.

It then starts the helper with `ShellExecuteExW` and the `runas` verb, keeps the process handle, and waits for a connection while also watching that handle. Once a client connects:

1. the interface checks that `GetNamedPipeClientProcessId` is the process it started, and that the process runs the helper's program (`QueryFullProcessImageNameW`);
2. the helper, before it says anything, checks that `GetNamedPipeServerProcessId` is the process id on its command line, and that the process runs `bytescape.exe` from the helper's own folder.

The helper opens the pipe at the anonymous impersonation level. Without that, the process that owns the pipe could act with the helper's elevated rights through the connection.

A different administrator account answering the prompt ("over the shoulder") runs the helper as another user, whose identifier the pipe does not admit, so the connection is refused. We kept it that way on purpose; admitting more would let other accounts' processes reach the pipe.

## The conversation

Frames are `length (u32) | version (u8) | kind (u8) | fields`, little-endian. The details are in the `bs-ipc` crate documentation. In short:

1. The interface sends a hello with the launch value. The helper answers with its own hello carrying the same value. A wrong value or version ends the run with no answer.
2. The interface sends one request: a scan or a delete.
3. The helper streams batches and progress (scan) or one outcome per item (delete), and ends with a done message or an error.
4. The helper exits. It also exits the moment the connection closes, after 20 seconds without a hello or a request, and when one write to the interface has been stuck for 60 seconds.

Limits: a frame body is at most 4 MiB, and the helper reads requests of at most 1 MiB. A path is at most 32 KiB, a delete request has at most 8192 items. The decoder never allocates for a count or length before the data that backs it has arrived. A malformed frame, an unknown version or kind, or a message that is not valid at its place makes the helper exit.

## The promises, and what shows each one

Test names below are in `crates/bytescape-helper/tests/` unless a crate is named. Run them with `cargo test -p bytescape-helper -p bs-ipc -p bs-fsops`.

### The channel

| Promise | Shown by |
|---|---|
| Wrong command line: no work, no output | `protocol::a_wrong_command_line_ends_the_helper_with_no_work_and_no_output`, `args` unit tests |
| Needs a valid `PKEXEC_UID` and a parent of that user | `protocol::without_a_valid_pkexec_uid_the_helper_does_not_start`, `linux::tests::only_a_plain_number_other_than_zero_names_a_user`, `root::the_helper_refuses_a_parent_that_is_not_the_user_it_was_told_about` |
| Standard input and output must be pipes | `protocol::standard_input_and_output_must_be_pipes` |
| Wrong launch value or version: exit, no answer | `protocol::a_wrong_launch_value_ends_the_helper_without_an_answer`, `protocol::another_protocol_version_ends_the_helper_without_an_answer`, `session::a_wrong_hello_gets_no_answer_and_no_work` |
| No work before the hello | `protocol::a_request_before_the_hello_does_no_work` |
| Broken frames end the run | `protocol::broken_frames_end_the_helper`, `bs-ipc` tests (`bad_lengths_are_refused_before_the_body_is_read`, `an_unknown_version_is_refused`, `an_unknown_kind_is_refused`) |
| The decoder survives any bytes, with bounded memory | `bs-ipc` property tests (`arbitrary_bodies_never_panic`, `arbitrary_fields_never_panic`, `arbitrary_streams_end`, `damaged_frames_are_handled`), `counts_are_checked_against_the_data_before_anything_is_reserved`, `wire::tests` |
| The helper never decodes what it would not accept | `bs-ipc` `the_helper_decodes_only_requests` |
| One request, then exit | `protocol::only_one_request_is_served` |
| A run serves only the kind of work the user authorized | `protocol::a_run_serves_only_the_kind_of_request_it_was_started_for`, `elevate::a_delete_asked_of_a_scan_helper_is_never_done` |
| Exits on a closed connection, on silence, on a stuck write | `protocol::a_closed_connection_ends_the_helper_at_once`, `protocol::a_helper_that_hears_nothing_gives_up`, `session::a_write_that_stays_blocked_trips_the_watchdog` |
| The policy matches the code | `policy` tests |
| Which helper is started: not from an AppImage, the snap's own only when it is classic, and only a helper that only the administrator can change | `bs-platform` `elevate::linux::tests` (`a_helper_that_someone_else_can_change_is_not_started`, `an_appimage_cannot_elevate_and_says_why`, `a_classic_snap_uses_the_helper_inside_it`, `the_snap_variable_is_the_users_to_set_so_the_helper_it_names_is_checked`, `the_helper_is_checked_again_when_it_is_launched`, `a_forged_snap_of_writable_folders_with_a_link_starts_only_the_file_that_was_checked`) |
| `pkexec` is the system's, found in a fixed place and not on the `PATH` | `bs-platform` `elevate::linux::tests` (`the_systems_pkexec_is_the_set_user_id_program_in_the_administrators_folders`, `a_pkexec_that_is_not_the_administrators_is_not_used`, `a_pkexec_on_the_path_is_never_used`) |
| The policy gives the helper no display variables | `policy::both_actions_name_the_installed_helper_and_their_first_argument` |
| The interface refuses to run elevated | `bs-platform` `elevation::tests` |
| Windows: pipe name, parent process, program, access list, first instance | `windows_launch` tests, `bs-fsops` `winpipe` tests, `bs-platform` `elevate::windows` tests (all run on the Windows runner) |
| Linux launch end to end, including a declined prompt and an impostor | `elevate` tests (through a stand-in for `pkexec`) |

### Deleting

| Promise | Shown by |
|---|---|
| Blocked paths are always refused, even with a confirmation, in every spelling, without touching the disk | `delete::tests::a_blocked_root_is_refused_in_every_spelling_without_touching_the_disk`, `a_confirmation_never_unblocks_a_blocked_path`, `blocked_places_are_blocked_in_every_spelling`, `protocol::the_operating_systems_own_places_are_refused_whatever_the_interface_says`, `root::as_root_the_operating_systems_places_are_still_refused` |
| The boot loader and recovery are blocked wherever the system keeps them: `/boot` and `/efi` (the partition systemd mounts there), and on Windows `\Boot`, `\EFI` and `\Recovery` of any drive, with the boot manager file | `bs-knowledge` `every_listed_os_root_is_blocked_with_and_without_an_environment`, `names_that_only_look_like_the_boot_places_are_not_blocked`, `delete::tests::the_boot_loader_is_refused_wherever_the_system_mounts_it` |
| A folder that holds a blocked place is blocked | `delete::tests::a_folder_that_holds_a_blocked_place_is_blocked_too` |
| The real place is judged, not only the text | `delete::tests::the_real_place_is_judged_as_well_as_the_text`, `a_path_that_goes_through_a_link_is_refused_not_followed`, `spellings_of_the_same_place_that_differ_are_not_accepted` |
| Protected paths need the request's own confirmation for that path | `delete::tests::a_protected_path_needs_its_own_confirmation` |
| The strictest rule wins across the request | `delete::tests::a_blocked_path_is_refused_and_takes_the_whole_request_with_it`, `paths_that_make_no_sense_refuse_the_whole_request` |
| Items inside other items, and repeats, are left to the outer one | `delete::tests::an_item_inside_another_item_is_left_to_the_outer_one` |
| A changed item is left alone | `delete::tests::an_item_that_is_not_what_was_seen_is_left_alone`, `bs-fsops` `target::tests::an_item_replaced_since_it_was_opened_is_left_alone` |
| Links are deleted as links and never followed; a folder swapped for a link mid-delete never leads out | `delete::tests::a_link_is_deleted_as_a_link_and_never_followed`, `swapping_a_folder_for_a_link_during_the_delete_never_reaches_outside`, `bs-fsops` `a_link_swapped_in_for_the_folder_is_not_followed` |
| A moved folder is not acted in | `bs-fsops` `target::tests::a_folder_moved_since_it_was_opened_is_not_acted_in` |
| Other filesystems and bind mounts are never entered | `mounts_namespace` tests (private mount namespace) |
| A partial delete is reported as a failure, with what was removed | `delete::tests::a_failure_part_way_is_reported_as_a_failure_with_what_was_removed`, `mounts_namespace::a_mount_point_is_never_deleted_and_nothing_behind_it_is_touched` |
| A cut connection never looks like a finished delete | `bs-ipc` `a_delete_cut_short_is_never_reported_as_complete`, `a_delete_end_before_every_item_has_an_outcome_is_refused` |
| Only permanent deletes | `delete::tests::a_request_that_is_not_permanent_deletes_nothing`, `protocol::a_request_that_is_not_permanent_is_refused_item_by_item` |
| A hang-up stops the delete between items | `protocol::closing_the_connection_mid_delete_stops_the_delete`, `delete::tests::stopping_between_items_leaves_the_rest_alone` |
| No account entry, no administrator delete; the refusal has a code of its own (`NoAccount`) so the interface can say why. Only the user's own line is read as text: damage or other encodings elsewhere in the file are no reason to refuse | `linux::tests::an_account_with_no_passwd_entry_gets_a_judge_that_refuses_every_delete`, `lines_of_other_users_may_be_any_bytes`, `damage_in_other_lines_does_not_refuse_the_user_with_a_good_line`, `session::a_judge_that_cannot_be_trusted_refuses_the_delete_and_touches_nothing`, `bs-ipc` `every_fallback_and_error_code_round_trips_and_an_unknown_one_is_refused` |
| A replacement that gets the old item's inode number is told apart by its creation time (where the file system keeps one) | `bs-fsops` `target::tests::a_replacement_that_gets_the_same_inode_back_is_still_caught`, `an_item_keeps_its_identity_when_it_is_changed_and_renamed_but_not_replaced` |
| Names are compared by their exact 16-bit units on Windows, and the interface hands the shell the units of the path that was chosen, not text made from them (a name with a lone surrogate is not recycled as its look-alike) | `bs-platform` `delete::winpure::tests` (`the_shell_is_asked_for_the_item_that_was_chosen_and_not_for_a_look_alike`, `inside_and_the_drive_are_judged_by_exact_units`, `the_info_file_keeps_an_extension_that_is_not_text`), `bs-fsops` `winpath::tests` (`an_unpaired_surrogate_is_not_the_replacement_character` and the others), `delete::tests_windows::a_name_with_an_unpaired_surrogate_never_reaches_its_look_alike` (Windows runner) |
| It works with the administrator's rights where the user has none | `root` tests (privileged run) |

Every test that deletes works inside a folder made for it. The test helpers refuse paths outside that folder. The tests that name operating system folders use paths that do not exist, so even a mistake would have nothing to delete, and they check that first.

### Scanning

| Promise | Shown by |
|---|---|
| The tree equals the walker's | `protocol::a_scan_streams_the_same_tree_the_walker_builds` |
| The volume must be an absolute, plain, real folder | `protocol::a_volume_that_is_not_a_plain_absolute_folder_is_a_bad_request` |
| The user's places come from the request, not the helper's environment, which is left as it is | `protocol::the_users_places_come_from_the_request_and_never_from_the_helpers_environment`, `places::tests::the_environment_holds_the_requests_places_and_nothing_else` |
| A raw reader is used only for the kind of file system it reads, and only where it reads it correctly: ext4 and XFS at a mount point (ext4 from the device the mount table names, always with the mount's root; XFS only for a whole file system), btrfs at a mount point of a subvolume, NTFS for the root of a fixed NTFS drive | `plan::tests::linux` (`a_whole_ext4_mount_is_read_raw_with_what_the_mount_table_says`, `a_mount_of_part_of_a_filesystem_hands_its_root_to_the_reader`, `the_reader_follows_the_filesystem_type_of_the_mount`, `everything_else_is_walked_and_an_explicit_request_says_why`, `only_block_devices_under_dev_pass_the_real_check`), `plan::tests::what_is_not_a_drive_root_or_not_ntfs_is_walked_and_an_explicit_request_says_why` |
| The Windows volume that is opened comes from the helper's own drive list and never from the request | `plan::tests::a_path_from_the_request_never_becomes_the_volume_that_is_opened`, `the_volume_is_named_from_the_drive_list_and_nothing_else` |
| The NTFS reader's memory is limited to half of what the system reports as available | `plan::tests::the_ntfs_reader_is_used_for_a_drive_root_with_the_memory_the_system_reports` |
| A raw reader or a mount from the mount table is used only for the mount that `statx` says the scan root is on (a mount placed over a shorter path hides the mounts below it, and the table still lists them), and an ext4 device only when its device number is the one the table shows for the mount; otherwise the walker reads the folder and no other mount is entered. | `plan::tests::linux::a_mount_the_scan_root_is_not_on_is_not_the_one_chosen`, `a_device_is_read_raw_only_when_it_is_the_one_the_table_shows_for_the_mount`, `a_real_block_device_passes_only_with_its_own_number`, `mounts_namespace::a_mount_hidden_by_a_mount_over_a_shorter_path_is_not_the_one_the_scan_uses` (real mounts; needs the right to mount) |
| Only mounts of the scan's own file system may be entered, and not one that would show files the scan already counts, judged from where the scan starts (the whole filesystem mounted elsewhere, which shows the start again, and bind mounts of folders inside a mount the scan reaches), whatever the request lists; stacked mounts are told apart by what is mounted on what, not by their numbers | `plan::tests::linux::only_mounts_of_the_scan_roots_own_filesystem_may_be_entered`, `plan::tests::linux::a_mount_that_another_mount_already_shows_is_not_entered`, `plan::tests::linux::the_scans_start_decides_which_mounts_cover_which`, `mounts::tests::mounts_that_show_what_the_scan_reaches_anyway_are_dropped`, `mounts::tests::the_scans_own_start_decides_not_the_drives_main_mount`, `mounts::tests::the_mount_in_view_is_the_one_nothing_is_stacked_on_whatever_the_ids`, `mounts::tests::mounts_of_one_filesystem_are_told_from_others_by_device_and_type` |
| Too large, partial, a mount that cannot be placed, or unreadable: the walker runs and the answer says why. A reader that fails after it has sent something is not repeated by the walker | `plan::tests` (`what_a_reader_reports_maps_to_what_the_interface_is_told`, `a_reader_that_refuses_before_sending_leaves_the_walker_to_do_the_scan_and_says_why`, `a_reader_that_fails_after_sending_is_not_repeated_by_the_walker`, `a_filesystem_that_needs_too_much_memory_falls_back_to_the_walker`, `a_mount_of_part_of_a_filesystem_and_a_non_ext4_device_fall_back_too`) |
| Large batches are cut to fit a frame, in order | `scan::tests::a_big_batch_is_cut_in_order_and_every_part_fits_a_frame` |
| A goodbye stops a scan; a hang-up ends it at once | `protocol::goodbye_stops_a_running_scan_and_it_ends_cancelled`, `closing_the_connection_mid_scan_ends_the_helper_at_once` |

### The helper starts nothing

`no_spawn::nothing_the_helper_links_can_start_a_program` reads the source of the helper and of every crate of this repository it links, on Linux and on Windows. It fails on any of these names appearing in code:

- the `Command` and `CommandExt` types, `CreateProcess*` (including `CreateProcessAsUser*`, `CreateProcessWithLogonW`, `CreateProcessWithTokenW` and `CreateProcessInternal*`), `NtCreateProcess*`, `NtCreateUserProcess`, `RtlCreateUserProcess`, `ShellExecute*` and `WinExec`;
- the C library's `exec*`, `fexecve`, `posix_spawn*`, `fork`, `vfork`, `clone`, `clone3`, `system` and `popen`, and the Windows C runtime's `_exec*`, `_wexec*`, `_spawn*`, `_wspawn*`, `_wsystem`, `_popen` and `_wpopen`, whether written as `libc::name`, imported by name from `libc` (also inside `use libc::{…}`), or declared by hand in an `extern` block;
- the system call numbers `SYS_execve`, `SYS_execveat`, `SYS_fork`, `SYS_vfork`, `SYS_clone` and `SYS_clone3`, which `libc::syscall` takes;
- a `libc::*` glob or a `libc as …` alias, which would hide the names above behind a shorter path.

Comments, strings and character literals are not read as code (raw, byte and C strings included). Test-only code is skipped, and only that: after a `#[cfg(test)]` or `#[cfg(all(…, test, …))]` (or an `any(…)` of test-only terms) the one thing the attribute applies to is left out, wherever it sits in the file. That is an item, a statement, or a field, variant, match arm, parameter or struct-literal field (which end at their comma, or just before the bracket that closes the list). The rest of the file is read. An inner `#![cfg(test)]` leaves out the scope it is in: the file when it is at the top of the file, otherwise the inline module. A file is skipped only when a `mod name;` line (or one with `#[path = "…"]`) that sits under such a condition brings it in, or when a skipped file brings it in. A file that nothing declares, or that is declared where the scan does not follow (inside an inline module), is read, whatever its name.

The same test checks its own reading with a case for each shape above, and with spawns planted after each of them. The helper's `clippy.toml` forbids the same calls at compile time, for the helper's own code, the Win32 and C library names among them. The test is `no_spawn::the_windows_ways_of_starting_a_program_are_all_seen`; it is the source scan that also covers the crates the helper links. The standard library itself contains such code; nothing the helper links calls it.

A name search cannot see a system call number passed as a plain integer, so the direct system calls are kept few and are listed here for review: `close_range` when the helper starts (it closes inherited descriptors), and `getdents64` in the directory walker.

The directory walker also has an io_uring path (`io_uring_setup`, `io_uring_enter` and `io_uring_register`, in `bs-scan`'s `linux/uring.rs`). It exists only behind the `iouring-bench` feature of `bs-scan`, which is off by default, which no crate of this repository turns on, and which no release build enables, so none of those calls is in the helper that ships. `cargo xtask helper-deps` also fails if `bs-scan`'s `iouring-bench` feature appears in the helper's feature tree on any target.

The Windows helper is linked with `/DEPENDENTLOADFLAG:0x800` (`DependentLoadFlags` = `LOAD_LIBRARY_SEARCH_SYSTEM32`), so it resolves its statically linked libraries from the system folder only, never from the folder it sits in: a DLL placed beside it is not picked up for those imports (the helper makes no run-time library loads). This does not protect the helper file itself in a folder the user can write to; the portable-zip note about where to unzip still applies.

### Dependencies

`cargo tree -p bytescape-helper -e normal` shows only this repository's crates (`bs-fsops`, `bs-ipc`, `bs-knowledge`, `bs-scan`, `bs-tree`; on Linux also `bs-scan-ext4`, `bs-scan-btrfs` and `bs-scan-xfs`, on Windows also `bs-scan-ntfs`), `libc` on Linux, and `windows-sys` with `windows-link` on Windows. `xtask/helper-deps.allow` lists them with a reason each, and `cargo xtask helper-deps` fails on anything else. No crate that can open a socket is in the closure. The `bs-platform` crate, which holds the launchers and the shell handoffs, is not.

## Things to know

- **The ordinary user can read names and sizes of the whole drive through a full scan.** That is the point of the feature, and it takes an administrator's authentication.
- **The raw readers read a mounted file system.** What they see can change under them; each is built to tolerate that, treats the data as hostile, and is limited to half of the memory that is available. A reader that cannot place a mount of the drive, or would need too much memory, hands the scan to the walker, and the answer says why.
- **Checks have small windows.** A path is judged, then opened, then judged again, and the delete acts through the folder that was opened. A change between those steps is detected by comparing identity and place, but this is not a lock. An attacker who could move the folder in that instant only makes the helper refuse.
- **On Linux an item's identity is its device, its inode number and, where the file system keeps one, its creation time.** The inode number alone is handed on to the next file that is created, so a deleted and replaced item would look the same; the creation time of the new file is later. File system clocks tick every few milliseconds, so a replacement made within the same tick is not told apart. Where the file system reports no creation time, the inode and device are all there is.
- **Process identifiers can be reused.** On Windows the interface keeps the helper's process handle, and the helper checks the pipe's server process, so a reused identifier does not help.
- **The helper writes no crash report.** It would need the platform crate, which the helper does not link. A panic ends the process at once with a fixed exit status and no text, because a message could carry a path.
- **A portable copy of the helper in a folder the user can write to** can be replaced by another program of the same user. The elevation prompt then names that program, and the user decides. The installed copies sit in folders only an administrator can write. The portable build says this to the person who unzips it (`packaging/windows/portable-readme.txt`): put both programs in a folder only administrators can write to, or use the installer. On Windows the interface does not check the folder of the helper; the check described above is for Linux.
- **A kept scan authorization can be reused by another process of the same user for a few minutes.** The scan policy action lets `pkexec` remember the authentication for the session. Since polkit 127, a remembered authentication also covers new processes that share the same user, parent, control group and terminal, so another program in the interface's session could start a *scan* helper without a new prompt for about five minutes. A scan only reads names and sizes, which that program could mostly learn as the user, and the helper still serves nothing but a scan on that authorization: deleting asks every time.
- **An account that is not in `/etc/passwd` cannot delete as administrator.** The helper learns the user's home folder, which the protected-path table needs, from the local account file, and never from the name services (looked up as the superuser, they could reach the network). If the user's own line is missing, or does not name an absolute home folder, a delete request is answered with a refusal (`NoAccount`) that says so, and nothing is touched. Other lines are not read as text, so names in other encodings elsewhere in the file do no harm. Scans are not affected.
- **Windows delete treats 8.3 short names and subst or mapped drives strictly.** The path sent must be the real path; a short name, a mapped drive or a different spelling of the place is refused, not guessed at. File names are compared as the 16-bit units Windows stores, so a name with an unpaired surrogate never opens, or counts as the same place as, a similar name with the replacement character.

## Running the privileged tests

Most tests run as an ordinary user. Three groups need more, and each prints `NOT RUN` and passes where it cannot do its job (so the script that runs them fails on that text):

- `mounts_namespace` needs the right to mount. Run it with `unshare --user --map-root-user --mount`, or as root.
- `root` needs the superuser and `BYTESCAPE_DISPOSABLE_HOST=1`. It makes a set-user-id copy of `env` as the stand-in for `pkexec`, so use it only on a machine that may be changed.
- `windows_launch` and the Windows unit tests need Windows. The round trip through the elevation prompt needs `BYTESCAPE_DISPOSABLE_HOST=1` and an account that can elevate without being asked.
