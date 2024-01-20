pub mod music_storage {
    pub mod library;
    pub mod music_collection;
    pub mod playlist;
    mod utils;

    #[allow(dead_code)]
    pub mod db_reader;
}

pub mod music_controller{
    pub mod controller;
}

pub mod music_player;
pub mod config {
    pub mod config;
    pub mod other_settings;
}
