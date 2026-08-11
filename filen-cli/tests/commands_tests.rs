use assert_fs::prelude::*;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	fs::{HasName, categories::NonRootFileType},
	io::client_impl::IoSharedClientExt as _,
};
use predicates::prelude::PredicateBooleanExt as _;
use rand::TryRngCore;
use test_utils::authenticated_cli_with_args;

#[shared_test_runtime]
async fn cmd_cd() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir = &resources.dir;

	// cd into existing directory
	authenticated_cli_with_args!("cd", test_dir.name().unwrap()).success();

	// try to cd into non-existing directory
	authenticated_cli_with_args!("cd", "/non_existing_directory")
		.failure()
		.stdout(predicates::str::contains("No such directory"));
}

#[shared_test_runtime]
async fn cmd_ls() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call ls on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	client.upload_file(file, &[]).await.unwrap();

	// ls
	authenticated_cli_with_args!("ls", test_dir.name().unwrap())
		.success()
		.stdout(predicates::str::contains("testfile.txt"));
}

#[shared_test_runtime]
async fn cmd_cat() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call cat on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let content = "Hello, Filen!";
	client.upload_file(file, content.as_bytes()).await.unwrap();

	// cat
	authenticated_cli_with_args!("cat", &format!("{}/testfile.txt", test_dir.name().unwrap()))
		.success()
		.stdout(predicates::str::contains(content));
}

#[shared_test_runtime]
async fn cmd_head_tail() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call head/tail on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let content = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\n";
	client.upload_file(file, content.as_bytes()).await.unwrap();

	// head
	authenticated_cli_with_args!(
		"head",
		&format!("{}/testfile.txt", test_dir.name().unwrap()),
		"-n1"
	)
	.success()
	.stdout(predicates::str::contains("Line 1").and(predicates::str::contains("Line 2").not()));

	// tail
	authenticated_cli_with_args!(
		"tail",
		&format!("{}/testfile.txt", test_dir.name().unwrap()),
		"-n1"
	)
	.success()
	.stdout(predicates::str::contains("Line 5").and(predicates::str::contains("Line 4").not()));
}

#[shared_test_runtime]
async fn cmd_stat() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call stat on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let mut contents = vec![0u8; 1024];
	rand::rng().try_fill_bytes(&mut contents).unwrap();
	client.upload_file(file, &contents).await.unwrap();

	// stat
	authenticated_cli_with_args!(
		"stat",
		&format!("{}/testfile.txt", test_dir.name().unwrap())
	)
	.success()
	.stdout(predicates::str::contains("1 KiB"));

	// stat on root drive
	authenticated_cli_with_args!("stat", "/")
		.success()
		.stdout(predicates::str::contains("Drive"));
}

#[shared_test_runtime]
async fn cmd_mkdir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let new_dir_name = "new_test_dir";

	// mkdir
	authenticated_cli_with_args!(
		"mkdir",
		&format!("{}/{}", test_dir.name().unwrap(), new_dir_name)
	)
	.success()
	.stdout(predicates::str::contains("Directory created"));

	// verify dir was created
	let created_dir = client
		.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), new_dir_name))
		.await
		.unwrap();
	assert!(created_dir.is_some());

	// mkdir -r
	let nested_dir_path = format!("{}/parent_dir/nested_dir", test_dir.name().unwrap());
	authenticated_cli_with_args!("mkdir", "-r", &nested_dir_path)
		.success()
		.stdout(predicates::str::contains("Directory created"));

	// verify nested dir was created
	let created_nested_dir = client.find_item_at_path(&nested_dir_path).await.unwrap();
	assert!(created_nested_dir.is_some());
}

#[shared_test_runtime]
async fn cmd_rm() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call rm on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let content = "Hello, Filen!";
	client.upload_file(file, content.as_bytes()).await.unwrap();

	// create test directory to call rm on
	client
		.create_dir(&test_dir.into(), "testdir_to_delete")
		.await
		.unwrap();

	// rm
	authenticated_cli_with_args!("rm", &format!("{}/testfile.txt", test_dir.name().unwrap()))
		.success()
		.stdout(predicates::str::contains("Trashed file"));
	authenticated_cli_with_args!(
		"rm",
		&format!("{}/testdir_to_delete", test_dir.name().unwrap())
	)
	.success()
	.stdout(predicates::str::contains("Trashed directory"));

	// verify file was deleted
	let deleted_file = client
		.find_item_at_path(&format!("{}/testfile.txt", test_dir.name().unwrap()))
		.await
		.unwrap();
	assert!(deleted_file.is_none());

	// verify directory was deleted
	let deleted_dir = client
		.find_item_at_path(&format!("{}/testdir_to_delete", test_dir.name().unwrap()))
		.await
		.unwrap();
	assert!(deleted_dir.is_none());
}

#[shared_test_runtime]
async fn cmd_mv() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let base = test_dir.name().unwrap();

	let exists = async |path: &str| {
		client
			.find_item_at_path(path)
			.await
			.unwrap_or_else(|e| panic!("failed to look up {}: {}", path, e))
			.is_some()
	};

	// create a source and a destination directory
	let src_dir = client.create_dir(&test_dir.into(), "mv_src").await.unwrap();
	let dest_dir = client
		.create_dir(&test_dir.into(), "mv_dest")
		.await
		.unwrap();
	for (parent, name) in [
		(&src_dir, "moved.txt"),
		(&src_dir, "renamed.txt"),
		(&src_dir, "in_place.txt"),
		(&dest_dir, "occupied.txt"),
	] {
		let file = client.make_file_builder(name, parent.uuid).unwrap();
		client.upload_file(file, b"Hello, Filen!").await.unwrap();
	}
	// a directory (with a child, to verify its contents come along) to move and rename
	let moved_dir = client
		.create_dir(&(&src_dir).into(), "moved_dir")
		.await
		.unwrap();
	let child = client
		.make_file_builder("child.txt", moved_dir.uuid)
		.unwrap();
	client.upload_file(child, b"child").await.unwrap();

	// destination is an existing directory: move the file into it, keeping its name
	authenticated_cli_with_args!(
		"mv",
		&format!("{}/mv_src/moved.txt", base),
		&format!("{}/mv_dest", base)
	)
	.success()
	.stdout(predicates::str::contains("Moved"));
	assert!(!exists(&format!("{}/mv_src/moved.txt", base)).await);
	assert!(exists(&format!("{}/mv_dest/moved.txt", base)).await);

	// destination is a file path in another directory: move and rename the file
	authenticated_cli_with_args!(
		"mv",
		&format!("{}/mv_src/renamed.txt", base),
		&format!("{}/mv_dest/new_name.txt", base)
	)
	.success()
	.stdout(predicates::str::contains("Moved"));
	assert!(!exists(&format!("{}/mv_src/renamed.txt", base)).await);
	assert!(exists(&format!("{}/mv_dest/new_name.txt", base)).await);

	// destination is a file path in the same directory: rename the file in place
	authenticated_cli_with_args!(
		"mv",
		&format!("{}/mv_src/in_place.txt", base),
		&format!("{}/mv_src/in_place_renamed.txt", base)
	)
	.success()
	.stdout(predicates::str::contains("Moved"));
	assert!(!exists(&format!("{}/mv_src/in_place.txt", base)).await);
	assert!(exists(&format!("{}/mv_src/in_place_renamed.txt", base)).await);

	// source is a directory: move and rename it, contents included
	authenticated_cli_with_args!(
		"mv",
		&format!("{}/mv_src/moved_dir", base),
		&format!("{}/mv_dest/renamed_dir", base)
	)
	.success()
	.stdout(predicates::str::contains("Moved"));
	assert!(!exists(&format!("{}/mv_src/moved_dir", base)).await);
	assert!(exists(&format!("{}/mv_dest/renamed_dir", base)).await);
	assert!(exists(&format!("{}/mv_dest/renamed_dir/child.txt", base)).await);

	// refuse to overwrite an existing destination
	authenticated_cli_with_args!(
		"mv",
		&format!("{}/mv_dest/moved.txt", base),
		&format!("{}/mv_dest/occupied.txt", base)
	)
	.failure()
	.stdout(predicates::str::contains("Destination already exists"));
	assert!(exists(&format!("{}/mv_dest/moved.txt", base)).await);

	// refuse a destination whose parent directory doesn't exist
	authenticated_cli_with_args!(
		"mv",
		&format!("{}/mv_dest/moved.txt", base),
		&format!("{}/no_such_dir/moved.txt", base)
	)
	.failure()
	.stdout(predicates::str::contains("No such destination directory"));
	assert!(exists(&format!("{}/mv_dest/moved.txt", base)).await);
}

#[shared_test_runtime]
async fn cmd_cp() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create a file and directory to copy
	let copy_file = client
		.make_file_builder("copy_file.txt", test_dir.uuid)
		.unwrap();
	client
		.upload_file(copy_file, b"copied content")
		.await
		.unwrap();
	let copy_dir = client
		.create_dir(&test_dir.into(), "copy_source")
		.await
		.unwrap();
	let nested_file = client
		.make_file_builder("nested.txt", copy_dir.uuid)
		.unwrap();
	client
		.upload_file(nested_file, b"nested content")
		.await
		.unwrap();

	let copy_target_dir = format!("{}/cp_target", test_dir.name().unwrap());
	client
		.create_dir(&test_dir.into(), "cp_target")
		.await
		.unwrap();

	// cp file
	authenticated_cli_with_args!(
		"cp",
		&format!("{}/copy_file.txt", test_dir.name().unwrap()),
		&copy_target_dir
	)
	.success()
	.stdout(predicates::str::contains("Copied"));
	assert!(
		client
			.find_item_at_path(&format!("{}/copy_file.txt", test_dir.name().unwrap()))
			.await
			.unwrap()
			.is_some()
	);
	assert!(
		client
			.find_item_at_path(&format!(
				"{}/cp_target/copy_file.txt",
				test_dir.name().unwrap()
			))
			.await
			.unwrap()
			.is_some()
	);

	// cp directory
	authenticated_cli_with_args!(
		"cp",
		&format!("{}/copy_source", test_dir.name().unwrap()),
		&copy_target_dir
	)
	.success()
	.stdout(predicates::str::contains("Copied"));
	assert!(
		client
			.find_item_at_path(&format!(
				"{}/cp_target/copy_source/nested.txt",
				test_dir.name().unwrap()
			))
			.await
			.unwrap()
			.is_some()
	);
}

#[shared_test_runtime]
async fn cmd_upload_download() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// build a local tree to upload: a loose file, and a directory with a nested file
	let local = assert_fs::TempDir::new().unwrap();
	local
		.child("single.txt")
		.write_str("single content")
		.unwrap();
	local
		.child("upload_source/nested")
		.create_dir_all()
		.unwrap();
	local
		.child("upload_source/nested/deep.txt")
		.write_str("deep content")
		.unwrap();

	// upload a single file
	authenticated_cli_with_args!(
		"upload",
		local.child("single.txt").path().to_str().unwrap(),
		test_dir.name().unwrap()
	)
	.success()
	.stdout(predicates::str::contains("Uploaded"));
	let uploaded = client
		.find_item_at_path(&format!("{}/single.txt", test_dir.name().unwrap()))
		.await
		.unwrap()
		.expect("uploaded file should exist");
	let NonRootFileType::File(uploaded) = uploaded else {
		panic!("uploaded item should be a file");
	};
	assert_eq!(
		String::from_utf8(client.download_file(uploaded.as_ref()).await.unwrap()).unwrap(),
		"single content"
	);

	// upload a directory recursively (into a subdirectory named after it)
	authenticated_cli_with_args!(
		"upload",
		local.child("upload_source").path().to_str().unwrap(),
		test_dir.name().unwrap()
	)
	.success()
	.stdout(predicates::str::contains("Uploaded"));
	assert!(
		client
			.find_item_at_path(&format!(
				"{}/upload_source/nested/deep.txt",
				test_dir.name().unwrap()
			))
			.await
			.unwrap()
			.is_some()
	);

	// download the directory back, recursively
	let download_target = assert_fs::TempDir::new().unwrap();
	authenticated_cli_with_args!(
		"download",
		&format!("{}/upload_source", test_dir.name().unwrap()),
		download_target.path().to_str().unwrap()
	)
	.success()
	.stdout(predicates::str::contains("Downloaded"));
	assert_eq!(
		std::fs::read_to_string(
			download_target
				.child("upload_source/nested/deep.txt")
				.path()
		)
		.unwrap(),
		"deep content"
	);

	// download a single file
	authenticated_cli_with_args!(
		"download",
		&format!("{}/single.txt", test_dir.name().unwrap()),
		download_target.path().to_str().unwrap()
	)
	.success()
	.stdout(predicates::str::contains("Downloaded"));
	assert_eq!(
		std::fs::read_to_string(download_target.child("single.txt").path()).unwrap(),
		"single content"
	);

	// try to upload a non-existing local path
	authenticated_cli_with_args!(
		"upload",
		local.child("does_not_exist").path().to_str().unwrap(),
		test_dir.name().unwrap()
	)
	.failure()
	.stdout(predicates::str::contains("No such local file or directory"));

	// try to download a non-existing remote path
	authenticated_cli_with_args!(
		"download",
		"/non_existing_file.txt",
		download_target.path().to_str().unwrap()
	)
	.failure()
	.stdout(predicates::str::contains("No such file or directory"));
}

#[shared_test_runtime]
async fn cmd_favorite_unfavorite() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call favorite on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let content = "Hello, Filen!";
	client.upload_file(file, content.as_bytes()).await.unwrap();

	let file_path = format!("{}/testfile.txt", test_dir.name().unwrap());

	// favorite
	authenticated_cli_with_args!("favorite", &file_path)
		.success()
		.stdout(predicates::str::contains("Favorited"));

	// verify file is favorited
	match client.find_item_at_path(&file_path).await.unwrap().unwrap() {
		NonRootFileType::File(file) => assert!(file.favorited),
		_ => panic!("Expected a file"),
	}

	// unfavorite
	authenticated_cli_with_args!("unfavorite", &file_path)
		.success()
		.stdout(predicates::str::contains("Unfavorited"));

	// verify file is unfavorited
	match client.find_item_at_path(&file_path).await.unwrap().unwrap() {
		NonRootFileType::File(file) => assert!(!file.favorited),
		_ => panic!("Expected a file"),
	}
}

#[shared_test_runtime]
async fn cmd_rclone() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call rclone on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let content = "Hello, Filen!";
	client.upload_file(file, content.as_bytes()).await.unwrap();

	// list file using rclone
	authenticated_cli_with_args!(
		"rclone",
		"lsf",
		&format!("filen:{}", test_dir.name().unwrap())
	)
	.success()
	.stdout(predicates::str::contains("testfile.txt"));
}

#[shared_test_runtime]
async fn cmd_list_trash_empty_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	// create test file to trash
	let test_dir = &resources.dir;
	let file = client
		.make_file_builder("testfile_from_cli_test_list_trash.txt", test_dir.uuid)
		.unwrap();
	let content = "Hello, Filen!";
	let mut file = client.upload_file(file, content.as_bytes()).await.unwrap();

	// trash the file
	client.trash_file(&mut file).await.unwrap();

	// list-trash
	authenticated_cli_with_args!("list-trash")
		.success()
		.stdout(predicates::str::contains(
			"testfile_from_cli_test_list_trash.txt",
		));

	// empty-trash
	authenticated_cli_with_args!("empty-trash")
		.success()
		.stdout(predicates::str::contains("Emptied trash"));

	// Verify our own file eventually leaves the trash listing. Asserting a globally
	// empty trash is impossible on the shared account (concurrent test binaries keep
	// trashing items), and server-side emptying is async and can lag by minutes.
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
	loop {
		let assert = authenticated_cli_with_args!("list-trash").success();
		let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
		if !stdout.contains("testfile_from_cli_test_list_trash.txt") {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!("file still listed in trash 300s after empty-trash:\n{stdout}");
		}
		tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	}
}
