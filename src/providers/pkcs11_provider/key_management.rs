// Copyright 2020 Contributors to the Parsec project.
// SPDX-License-Identifier: Apache-2.0
use super::{utils, KeyInfo, KeyPairType, LocalIdStore, Pkcs11Provider, ReadWriteSession, Session};
use crate::authenticators::ApplicationName;
use crate::key_info_managers::KeyTriple;
use crate::key_info_managers::{self, ManageKeyInfo};
use log::{error, info, trace, warn};
use parsec_interface::operations::psa_key_attributes::*;
use parsec_interface::operations::{
    psa_destroy_key, psa_export_public_key, psa_generate_key, psa_import_key,
};
use parsec_interface::requests::{ProviderID, ResponseStatus, Result};
use parsec_interface::secrecy::ExposeSecret;
use picky_asn1::wrapper::IntegerAsn1;
use picky_asn1_x509::RSAPublicKey;
use pkcs11::types::{CKR_OK, CK_ATTRIBUTE, CK_OBJECT_HANDLE, CK_SESSION_HANDLE};
use std::mem;

/// Gets a key identifier and key attributes from the Key Info Manager.
pub fn get_key_info(
    key_triple: &KeyTriple,
    store_handle: &dyn ManageKeyInfo,
) -> Result<([u8; 4], Attributes)> {
    match store_handle.get(key_triple) {
        Ok(Some(key_info)) => {
            if key_info.id.len() == 4 {
                let mut dst = [0; 4];
                dst.copy_from_slice(&key_info.id);
                Ok((dst, key_info.attributes))
            } else {
                error!("Stored Key ID is not valid.");
                Err(ResponseStatus::KeyInfoManagerError)
            }
        }
        Ok(None) => Err(ResponseStatus::PsaErrorDoesNotExist),
        Err(string) => Err(key_info_managers::to_response_status(string)),
    }
}

pub fn create_key_id(
    key_triple: KeyTriple,
    key_attributes: Attributes,
    store_handle: &mut dyn ManageKeyInfo,
    local_ids_handle: &mut LocalIdStore,
) -> Result<[u8; 4]> {
    let mut key_id = rand::random::<[u8; 4]>();
    while local_ids_handle.contains(&key_id) {
        key_id = rand::random::<[u8; 4]>();
    }
    let key_info = KeyInfo {
        id: key_id.to_vec(),
        attributes: key_attributes,
    };
    match store_handle.insert(key_triple.clone(), key_info) {
        Ok(insert_option) => {
            if insert_option.is_some() {
                if crate::utils::GlobalConfig::log_error_details() {
                    warn!("Overwriting Key triple mapping ({})", key_triple);
                } else {
                    warn!("Overwriting Key triple mapping");
                }
            }
            let _ = local_ids_handle.insert(key_id);

            Ok(key_id)
        }
        Err(string) => Err(key_info_managers::to_response_status(string)),
    }
}

pub fn remove_key_id(
    key_triple: &KeyTriple,
    key_id: [u8; 4],
    store_handle: &mut dyn ManageKeyInfo,
    local_ids_handle: &mut LocalIdStore,
) -> Result<()> {
    match store_handle.remove(key_triple) {
        Ok(_) => {
            let _ = local_ids_handle.remove(&key_id);
            Ok(())
        }
        Err(string) => Err(key_info_managers::to_response_status(string)),
    }
}

pub fn key_info_exists(key_triple: &KeyTriple, store_handle: &dyn ManageKeyInfo) -> Result<bool> {
    match store_handle.exists(key_triple) {
        Ok(val) => Ok(val),
        Err(string) => Err(key_info_managers::to_response_status(string)),
    }
}

impl Pkcs11Provider {
    /// Find the PKCS 11 object handle corresponding to the key ID and the key type (public,
    /// private or any key type) given as parameters for the current session.
    pub(super) fn find_key(
        &self,
        session: CK_SESSION_HANDLE,
        key_id: [u8; 4],
        key_type: KeyPairType,
    ) -> Result<CK_OBJECT_HANDLE> {
        let mut template = vec![CK_ATTRIBUTE::new(pkcs11::types::CKA_ID).with_bytes(&key_id)];
        match key_type {
            KeyPairType::PublicKey => template.push(
                CK_ATTRIBUTE::new(pkcs11::types::CKA_CLASS)
                    .with_ck_ulong(&pkcs11::types::CKO_PUBLIC_KEY),
            ),
            KeyPairType::PrivateKey => template.push(
                CK_ATTRIBUTE::new(pkcs11::types::CKA_CLASS)
                    .with_ck_ulong(&pkcs11::types::CKO_PRIVATE_KEY),
            ),
            KeyPairType::Any => (),
        }

        trace!("FindObjectsInit command");
        if let Err(e) = self.backend.find_objects_init(session, &template) {
            format_error!("Object enumeration init failed", e);
            Err(utils::to_response_status(e))
        } else {
            trace!("FindObjects command");
            match self.backend.find_objects(session, 1) {
                Ok(objects) => {
                    trace!("FindObjectsFinal command");
                    if let Err(e) = self.backend.find_objects_final(session) {
                        format_error!("Object enumeration final failed", e);
                        Err(utils::to_response_status(e))
                    } else if objects.is_empty() {
                        Err(ResponseStatus::PsaErrorDoesNotExist)
                    } else {
                        Ok(objects[0])
                    }
                }
                Err(e) => {
                    format_error!("Finding objects failed", e);
                    Err(utils::to_response_status(e))
                }
            }
        }
    }
    pub(super) fn psa_generate_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_generate_key::Operation,
    ) -> Result<psa_generate_key::Result> {
        if op.attributes.key_type != Type::RsaKeyPair {
            error!("The PKCS11 provider currently only supports creating RSA key pairs.");
            return Err(ResponseStatus::PsaErrorNotSupported);
        }

        let key_name = op.key_name;
        let key_attributes = op.attributes;

        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11, key_name);
        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        let mut local_ids_handle = self.local_ids.write().expect("Local ID lock poisoned");
        if key_info_exists(&key_triple, &*store_handle)? {
            return Err(ResponseStatus::PsaErrorAlreadyExists);
        }
        let key_id = create_key_id(
            key_triple.clone(),
            key_attributes,
            &mut *store_handle,
            &mut local_ids_handle,
        )?;

        let (mech, mut pub_template, mut priv_template, mut allowed_mechanism) =
            utils::parsec_to_pkcs11_params(key_attributes, &key_id)?;

        pub_template.push(utils::mech_type_to_allowed_mech_attribute(
            &mut allowed_mechanism,
        ));
        priv_template.push(utils::mech_type_to_allowed_mech_attribute(
            &mut allowed_mechanism,
        ));

        let session = Session::new(self, ReadWriteSession::ReadWrite).or_else(|err| {
            format_error!("Error creating a new session", err);
            remove_key_id(
                &key_triple,
                key_id,
                &mut *store_handle,
                &mut local_ids_handle,
            )?;
            Err(err)
        })?;

        if crate::utils::GlobalConfig::log_error_details() {
            info!(
                "Generating RSA key pair in session {}",
                session.session_handle()
            );
        }

        trace!("GenerateKeyPair command");
        match self.backend.generate_key_pair(
            session.session_handle(),
            &mech,
            &pub_template,
            &priv_template,
        ) {
            Ok(_key) => Ok(psa_generate_key::Result {}),
            Err(e) => {
                format_error!("Generate Key Pair operation failed", e);
                remove_key_id(
                    &key_triple,
                    key_id,
                    &mut *store_handle,
                    &mut local_ids_handle,
                )?;
                Err(utils::to_response_status(e))
            }
        }
    }

    pub(super) fn psa_import_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_import_key::Operation,
    ) -> Result<psa_import_key::Result> {
        if op.attributes.key_type != Type::RsaPublicKey {
            error!("The PKCS 11 provider currently only supports importing RSA public key.");
            return Err(ResponseStatus::PsaErrorNotSupported);
        }

        let key_name = op.key_name;
        let key_attributes = op.attributes;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11, key_name);
        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        let mut local_ids_handle = self.local_ids.write().expect("Local ID lock poisoned");
        if key_info_exists(&key_triple, &*store_handle)? {
            return Err(ResponseStatus::PsaErrorAlreadyExists);
        }
        let key_id = create_key_id(
            key_triple.clone(),
            key_attributes,
            &mut *store_handle,
            &mut local_ids_handle,
        )?;

        let mut template: Vec<CK_ATTRIBUTE> = Vec::new();

        let public_key: RSAPublicKey = picky_asn1_der::from_bytes(op.data.expose_secret())
            .or_else(|e| {
                format_error!("Failed to parse RsaPublicKey data", e);
                remove_key_id(
                    &key_triple,
                    key_id,
                    &mut *store_handle,
                    &mut local_ids_handle,
                )?;
                Err(ResponseStatus::PsaErrorInvalidArgument)
            })?;

        if public_key.modulus.is_negative() || public_key.public_exponent.is_negative() {
            error!("Only positive modulus and public exponent are supported.");
            remove_key_id(
                &key_triple,
                key_id,
                &mut *store_handle,
                &mut local_ids_handle,
            )?;
            return Err(ResponseStatus::PsaErrorInvalidArgument);
        }

        let modulus_object = &public_key.modulus.as_unsigned_bytes_be();
        let exponent_object = &public_key.public_exponent.as_unsigned_bytes_be();
        let bits = key_attributes.bits;
        if bits != 0 && modulus_object.len() * 8 != bits {
            if crate::utils::GlobalConfig::log_error_details() {
                error!(
                    "`bits` field of key attributes (value: {}) must be either 0 or equal to the size of the key in `data` (value: {}).",
                    key_attributes.bits,
                    modulus_object.len() * 8
                );
            } else {
                error!("`bits` field of key attributes must be either 0 or equal to the size of the key in `data`.");
            }
            return Err(ResponseStatus::PsaErrorInvalidArgument);
        }

        template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_CLASS)
                .with_ck_ulong(&pkcs11::types::CKO_PUBLIC_KEY),
        );
        template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_KEY_TYPE).with_ck_ulong(&pkcs11::types::CKK_RSA),
        );
        template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_TOKEN).with_bool(&pkcs11::types::CK_TRUE));
        template.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_MODULUS).with_bytes(modulus_object));
        template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_PUBLIC_EXPONENT).with_bytes(exponent_object),
        );
        template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_VERIFY).with_bool(&pkcs11::types::CK_TRUE));
        template
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_ENCRYPT).with_bool(&pkcs11::types::CK_TRUE));
        template.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_ID).with_bytes(&key_id));
        template.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_PRIVATE).with_bool(&pkcs11::types::CK_FALSE),
        );

        // Restrict to RSA.
        let allowed_mechanisms = [pkcs11::types::CKM_RSA_PKCS];
        // The attribute contains a pointer to the allowed_mechanism array and its size as
        // ulValueLen.
        let mut allowed_mechanisms_attribute =
            CK_ATTRIBUTE::new(pkcs11::types::CKA_ALLOWED_MECHANISMS);
        allowed_mechanisms_attribute.ulValueLen = mem::size_of_val(&allowed_mechanisms);
        allowed_mechanisms_attribute.pValue = &allowed_mechanisms
            as *const pkcs11::types::CK_MECHANISM_TYPE
            as pkcs11::types::CK_VOID_PTR;
        template.push(allowed_mechanisms_attribute);

        let session = Session::new(self, ReadWriteSession::ReadWrite).or_else(|err| {
            format_error!("Error creating a new session", err);
            remove_key_id(
                &key_triple,
                key_id,
                &mut *store_handle,
                &mut local_ids_handle,
            )?;
            Err(err)
        })?;

        if crate::utils::GlobalConfig::log_error_details() {
            info!(
                "Importing RSA public key in session {}",
                session.session_handle()
            );
        }

        trace!("CreateObject command");
        match self
            .backend
            .create_object(session.session_handle(), &template)
        {
            Ok(_key) => Ok(psa_import_key::Result {}),
            Err(e) => {
                format_error!("Import operation failed", e);
                remove_key_id(
                    &key_triple,
                    key_id,
                    &mut *store_handle,
                    &mut local_ids_handle,
                )?;
                Err(utils::to_response_status(e))
            }
        }
    }

    pub(super) fn psa_export_public_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_export_public_key::Operation,
    ) -> Result<psa_export_public_key::Result> {
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11, key_name);
        let store_handle = self.key_info_store.read().expect("Key store lock poisoned");
        let (key_id, _key_attributes) = get_key_info(&key_triple, &*store_handle)?;

        let session = Session::new(self, ReadWriteSession::ReadOnly)?;
        if crate::utils::GlobalConfig::log_error_details() {
            info!(
                "Export RSA public key in session {}",
                session.session_handle()
            );
        }

        let key = self.find_key(session.session_handle(), key_id, KeyPairType::PublicKey)?;
        info!("Located key for export.");

        let mut size_attrs: Vec<CK_ATTRIBUTE> = Vec::new();
        size_attrs.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_MODULUS));
        size_attrs.push(CK_ATTRIBUTE::new(pkcs11::types::CKA_PUBLIC_EXPONENT));

        // Get the length of the attributes to retrieve.
        trace!("GetAttributeValue command");
        let (modulus_len, public_exponent_len) =
            match self
                .backend
                .get_attribute_value(session.session_handle(), key, &mut size_attrs)
            {
                Ok((rv, attrs)) => {
                    if rv != CKR_OK {
                        format_error!("Error when extracting attribute", rv);
                        Err(utils::rv_to_response_status(rv))
                    } else {
                        Ok((attrs[0].ulValueLen, attrs[1].ulValueLen))
                    }
                }
                Err(e) => {
                    format_error!("Failed to read attributes from public key", e);
                    Err(utils::to_response_status(e))
                }
            }?;

        let mut modulus: Vec<pkcs11::types::CK_BYTE> = Vec::new();
        let mut public_exponent: Vec<pkcs11::types::CK_BYTE> = Vec::new();
        modulus.resize(modulus_len, 0);
        public_exponent.resize(public_exponent_len, 0);

        let mut extract_attrs: Vec<CK_ATTRIBUTE> = Vec::new();
        extract_attrs
            .push(CK_ATTRIBUTE::new(pkcs11::types::CKA_MODULUS).with_bytes(modulus.as_mut_slice()));
        extract_attrs.push(
            CK_ATTRIBUTE::new(pkcs11::types::CKA_PUBLIC_EXPONENT)
                .with_bytes(public_exponent.as_mut_slice()),
        );

        trace!("GetAttributeValue command");
        match self
            .backend
            .get_attribute_value(session.session_handle(), key, &mut extract_attrs)
        {
            Ok(res) => {
                let (rv, attrs) = res;
                if rv != CKR_OK {
                    format_error!("Error when extracting attribute", rv);
                    Err(utils::rv_to_response_status(rv))
                } else {
                    let modulus = attrs[0].get_bytes();
                    let public_exponent = attrs[1].get_bytes();

                    // To produce a valid ASN.1 RSAPublicKey structure, 0x00 is put in front of the positive
                    // integer if highest significant bit is one, to differentiate it from a negative number.
                    let modulus = IntegerAsn1::from_unsigned_bytes_be(modulus);
                    let public_exponent = IntegerAsn1::from_unsigned_bytes_be(public_exponent);

                    let key = RSAPublicKey {
                        modulus,
                        public_exponent,
                    };
                    let data = picky_asn1_der::to_vec(&key).map_err(|err| {
                        format_error!("Could not serialise key elements", err);
                        ResponseStatus::PsaErrorCommunicationFailure
                    })?;
                    Ok(psa_export_public_key::Result { data: data.into() })
                }
            }
            Err(e) => {
                format_error!("Failed to read attributes from public key", e);
                Err(utils::to_response_status(e))
            }
        }
    }

    pub(super) fn psa_destroy_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_destroy_key::Operation,
    ) -> Result<psa_destroy_key::Result> {
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::Pkcs11, key_name);
        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        let mut local_ids_handle = self.local_ids.write().expect("Local ID lock poisoned");
        let (key_id, _) = get_key_info(&key_triple, &*store_handle)?;

        let session = Session::new(self, ReadWriteSession::ReadWrite)?;
        if crate::utils::GlobalConfig::log_error_details() {
            info!(
                "Deleting RSA keypair in session {}",
                session.session_handle()
            );
        }

        match self.find_key(session.session_handle(), key_id, KeyPairType::Any) {
            Ok(key) => {
                trace!("DestroyObject command");
                match self.backend.destroy_object(session.session_handle(), key) {
                    Ok(_) => info!("Private part of the key destroyed successfully."),
                    Err(e) => {
                        format_error!("Failed to destroy private part of the key", e);
                        return Err(utils::to_response_status(e));
                    }
                };
            }
            Err(e) => {
                format_error!("Error destroying key", e);
                return Err(e);
            }
        };

        // Second key is optional.
        match self.find_key(session.session_handle(), key_id, KeyPairType::Any) {
            Ok(key) => {
                trace!("DestroyObject command");
                match self.backend.destroy_object(session.session_handle(), key) {
                    Ok(_) => info!("Private part of the key destroyed successfully."),
                    Err(e) => {
                        format_error!("Failed to destroy private part of the key", e);
                        return Err(utils::to_response_status(e));
                    }
                };
            }
            // A second key is optional.
            Err(ResponseStatus::PsaErrorDoesNotExist) => (),
            Err(e) => {
                format_error!("Error destroying key", e);
                return Err(e);
            }
        };

        remove_key_id(
            &key_triple,
            key_id,
            &mut *store_handle,
            &mut local_ids_handle,
        )?;

        Ok(psa_destroy_key::Result {})
    }
}
