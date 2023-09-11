use std::collections::{HashMap, HashSet};
use std::str;

use grenad::CompressionType;
use heed::types::ByteSlice;
use heed::{BytesDecode, BytesEncode, Database};
use log::debug;

use crate::error::SerializationError;
use crate::heed_codec::StrBEU16Codec;
use crate::index::main_key::WORDS_PREFIXES_FST_KEY;
use crate::update::index_documents::{
    create_sorter, merge_cbo_roaring_bitmaps, sorter_into_lmdb_database, valid_lmdb_key,
    CursorClonableMmap, MergeFn,
};
use crate::{CboRoaringBitmapCodec, Result};

pub struct WordPrefixIntegerDocids<'t, 'u, 'i> {
    wtxn: &'t mut heed::RwTxn<'i, 'u>,
    prefix_database: Database<StrBEU16Codec, CboRoaringBitmapCodec>,
    word_database: Database<StrBEU16Codec, CboRoaringBitmapCodec>,
    pub(crate) chunk_compression_type: CompressionType,
    pub(crate) chunk_compression_level: Option<u32>,
    pub(crate) max_nb_chunks: Option<usize>,
    pub(crate) max_memory: Option<usize>,
}

impl<'t, 'u, 'i> WordPrefixIntegerDocids<'t, 'u, 'i> {
    pub fn new(
        wtxn: &'t mut heed::RwTxn<'i, 'u>,
        prefix_database: Database<StrBEU16Codec, CboRoaringBitmapCodec>,
        word_database: Database<StrBEU16Codec, CboRoaringBitmapCodec>,
    ) -> WordPrefixIntegerDocids<'t, 'u, 'i> {
        WordPrefixIntegerDocids {
            wtxn,
            prefix_database,
            word_database,
            chunk_compression_type: CompressionType::None,
            chunk_compression_level: None,
            max_nb_chunks: None,
            max_memory: None,
        }
    }

    #[logging_timer::time("WordPrefixIntegerDocids::{}")]
    pub fn execute(
        self,
        new_word_integer_docids: grenad::Reader<CursorClonableMmap>,
        new_prefix_fst_words: &[String],
        common_prefix_fst_words: &[&[String]],
        del_prefix_fst_words: &HashSet<Vec<u8>>,
    ) -> Result<()> {
        puffin::profile_function!();
        debug!("Computing and writing the word levels integers docids into LMDB on disk...");

        let mut prefix_integer_docids_sorter = create_sorter(
            grenad::SortAlgorithm::Unstable,
            merge_cbo_roaring_bitmaps,
            self.chunk_compression_type,
            self.chunk_compression_level,
            self.max_nb_chunks,
            self.max_memory,
        );

        let mut new_word_integer_docids_iter = new_word_integer_docids.into_cursor()?;

        if !common_prefix_fst_words.is_empty() {
            // We fetch all the new common prefixes between the previous and new prefix fst.
            let mut buffer = Vec::new();
            let mut current_prefixes: Option<&&[String]> = None;
            let mut prefixes_cache = HashMap::new();
            while let Some((key, data)) = new_word_integer_docids_iter.move_on_next()? {
                let (word, pos) = StrBEU16Codec::bytes_decode(key).ok_or(heed::Error::Decoding)?;

                current_prefixes = match current_prefixes.take() {
                    Some(prefixes) if word.starts_with(&prefixes[0]) => Some(prefixes),
                    _otherwise => {
                        write_prefixes_in_sorter(
                            &mut prefixes_cache,
                            &mut prefix_integer_docids_sorter,
                        )?;
                        common_prefix_fst_words
                            .iter()
                            .find(|prefixes| word.starts_with(&prefixes[0]))
                    }
                };

                if let Some(prefixes) = current_prefixes {
                    for prefix in prefixes.iter() {
                        if word.starts_with(prefix) {
                            buffer.clear();
                            buffer.extend_from_slice(prefix.as_bytes());
                            buffer.push(0);
                            buffer.extend_from_slice(&pos.to_be_bytes());
                            match prefixes_cache.get_mut(&buffer) {
                                Some(value) => value.push(data.to_owned()),
                                None => {
                                    prefixes_cache.insert(buffer.clone(), vec![data.to_owned()]);
                                }
                            }
                        }
                    }
                }
            }

            write_prefixes_in_sorter(&mut prefixes_cache, &mut prefix_integer_docids_sorter)?;
        }

        // We fetch the docids associated to the newly added word prefix fst only.
        let db = self.word_database.remap_data_type::<ByteSlice>();
        for prefix_bytes in new_prefix_fst_words {
            let prefix = str::from_utf8(prefix_bytes.as_bytes()).map_err(|_| {
                SerializationError::Decoding { db_name: Some(WORDS_PREFIXES_FST_KEY) }
            })?;

            // iter over all lines of the DB where the key is prefixed by the current prefix.
            let iter = db
                .remap_key_type::<ByteSlice>()
                .prefix_iter(self.wtxn, prefix_bytes.as_bytes())?
                .remap_key_type::<StrBEU16Codec>();
            for result in iter {
                let ((word, pos), data) = result?;
                if word.starts_with(prefix) {
                    let key = (prefix, pos);
                    let bytes = StrBEU16Codec::bytes_encode(&key).unwrap();
                    prefix_integer_docids_sorter.insert(bytes, data)?;
                }
            }
        }

        // We remove all the entries that are no more required in this word prefix integer
        // docids database.
        // We also avoid iterating over the whole `word_prefix_integer_docids` database if we know in
        // advance that the `if del_prefix_fst_words.contains(prefix.as_bytes()) {` condition below
        // will always be false (i.e. if `del_prefix_fst_words` is empty).
        if !del_prefix_fst_words.is_empty() {
            let mut iter = self.prefix_database.iter_mut(self.wtxn)?.lazily_decode_data();
            while let Some(((prefix, _), _)) = iter.next().transpose()? {
                if del_prefix_fst_words.contains(prefix.as_bytes()) {
                    unsafe { iter.del_current()? };
                }
            }
            drop(iter);
        }

        // We finally write all the word prefix integer docids into the LMDB database.
        sorter_into_lmdb_database(
            self.wtxn,
            *self.prefix_database.as_polymorph(),
            prefix_integer_docids_sorter,
            merge_cbo_roaring_bitmaps,
        )?;

        Ok(())
    }
}

fn write_prefixes_in_sorter(
    prefixes: &mut HashMap<Vec<u8>, Vec<Vec<u8>>>,
    sorter: &mut grenad::Sorter<MergeFn>,
) -> Result<()> {
    for (key, data_slices) in prefixes.drain() {
        for data in data_slices {
            if valid_lmdb_key(&key) {
                sorter.insert(&key, data)?;
            }
        }
    }

    Ok(())
}